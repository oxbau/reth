use super::{
    find_fixed_range, LoadedJar, SnapshotJarProvider, SnapshotProviderRW, BLOCKS_PER_SNAPSHOT,
};
use crate::{
    to_range, BlockHashReader, BlockNumReader, BlockReader, BlockSource, HeaderProvider,
    ReceiptProvider, TransactionVariant, TransactionsProvider, TransactionsProviderExt,
    WithdrawalsProvider,
};
use dashmap::{mapref::one::RefMut, DashMap};
use parking_lot::RwLock;
use reth_db::{
    codecs::CompactU256,
    models::StoredBlockBodyIndices,
    snapshot::{iter_snapshots, HeaderMask, ReceiptMask, SnapshotCursor, TransactionMask},
};
use reth_interfaces::provider::{ProviderError, ProviderResult};
use reth_nippy_jar::NippyJar;
use reth_primitives::{
    snapshot::HighestSnapshots, Address, Block, BlockHash, BlockHashOrNumber, BlockNumber,
    BlockWithSenders, ChainInfo, Header, Receipt, SealedBlock, SealedBlockWithSenders,
    SealedHeader, SnapshotSegment, TransactionMeta, TransactionSigned, TransactionSignedNoHash,
    TxHash, TxNumber, Withdrawal, B256, U256,
};
use std::{
    collections::{hash_map::Entry, BTreeMap, HashMap},
    ops::{Range, RangeBounds, RangeInclusive},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::watch;

/// Alias type for a map that can be queried for transaction/block ranges from a block/transaction
/// segment respectively. It uses `BlockNumber` to represent the block end of a snapshot range or
/// `TxNumber` to represent the transaction end of a snapshot range.
///
/// Can be in one of the two formats:
/// - `HashMap<SnapshotSegment, BTreeMap<BlockNumber, RangeInclusive<TxNumber>>>`
/// - `HashMap<SnapshotSegment, BTreeMap<TxNumber, RangeInclusive<BlockNumber>>>`
type SegmentRanges = HashMap<SnapshotSegment, BTreeMap<u64, RangeInclusive<u64>>>;

/// [`SnapshotProvider`] manages all existing [`SnapshotJarProvider`].
#[derive(Debug, Default)]
pub struct SnapshotProvider {
    /// Maintains a map which allows for concurrent access to different `NippyJars`, over different
    /// segments and ranges.
    map: DashMap<(BlockNumber, SnapshotSegment), LoadedJar>,
    /// Available snapshot transaction ranges on disk indexed by max blocks.
    snapshots_max_block: RwLock<HashMap<SnapshotSegment, u64>>,
    /// Available snapshot block ranges on disk indexed by max transactions.
    snapshots_tx_index: RwLock<SegmentRanges>,
    /// Tracks the highest snapshot of every segment.
    highest_tracker: Option<watch::Receiver<Option<HighestSnapshots>>>,
    /// Directory where snapshots are located
    path: PathBuf,
    /// Whether [`SnapshotJarProvider`] loads filters into memory. If not, `by_hash` queries won't
    /// be able to be queried directly.
    load_filters: bool,
    /// Maintains a map of Snapshot writers for each [`SnapshotSegment`]
    writers: DashMap<SnapshotSegment, SnapshotProviderRW<'static>>,
}

impl SnapshotProvider {
    /// Creates a new [`SnapshotProvider`].
    pub fn new(path: impl AsRef<Path>) -> ProviderResult<Self> {
        let provider = Self {
            map: Default::default(),
            writers: Default::default(),
            snapshots_max_block: Default::default(),
            snapshots_tx_index: Default::default(),
            highest_tracker: None,
            path: path.as_ref().to_path_buf(),
            load_filters: false,
        };

        provider.update_index()?;
        Ok(provider)
    }

    /// Loads filters into memory when creating a [`SnapshotJarProvider`].
    pub fn with_filters(mut self) -> Self {
        self.load_filters = true;
        self
    }

    /// Adds a highest snapshot tracker to the provider
    pub fn with_highest_tracker(
        mut self,
        highest_tracker: Option<watch::Receiver<Option<HighestSnapshots>>>,
    ) -> Self {
        self.highest_tracker = highest_tracker;
        self
    }

    /// Gets the [`SnapshotJarProvider`] of the requested segment and block.
    pub fn get_segment_provider_from_block(
        &self,
        segment: SnapshotSegment,
        block: BlockNumber,
        path: Option<&Path>,
    ) -> ProviderResult<SnapshotJarProvider<'_>> {
        self.get_segment_provider(
            segment,
            || self.get_segment_ranges_from_block(segment, block),
            path,
        )?
        .ok_or_else(|| ProviderError::MissingSnapshotBlock(segment, block))
    }

    /// Gets the [`SnapshotJarProvider`] of the requested segment and transaction.
    pub fn get_segment_provider_from_transaction(
        &self,
        segment: SnapshotSegment,
        tx: TxNumber,
        path: Option<&Path>,
    ) -> ProviderResult<SnapshotJarProvider<'_>> {
        self.get_segment_provider(
            segment,
            || self.get_segment_ranges_from_transaction(segment, tx),
            path,
        )?
        .ok_or_else(|| ProviderError::MissingSnapshotTx(segment, tx))
    }

    /// Gets the [`SnapshotJarProvider`] of the requested segment and block or transaction.
    pub fn get_segment_provider(
        &self,
        segment: SnapshotSegment,
        fn_range: impl Fn() -> Option<RangeInclusive<BlockNumber>>,
        path: Option<&Path>,
    ) -> ProviderResult<Option<SnapshotJarProvider<'_>>> {
        // If we have a path, then get the block range and transaction range from its name.
        // Otherwise, check `self.available_snapshots`
        let block_range = match path {
            Some(path) => {
                SnapshotSegment::parse_filename(path.file_name().ok_or_else(|| {
                    ProviderError::MissingSnapshotPath(segment, path.to_path_buf())
                })?)
                .and_then(|(parsed_segment, block_range)| {
                    if parsed_segment == segment {
                        return Some(block_range)
                    }
                    None
                })
            }
            None => fn_range(),
        };

        // Return cached `LoadedJar` or insert it for the first time, and then, return it.
        if let Some(block_range) = block_range {
            return Ok(Some(self.get_or_create_jar_provider(segment, &block_range)?))
        }

        Ok(None)
    }

    /// Given a segment, block range and transaction range it returns a cached
    /// [`SnapshotJarProvider`]. TODO: we should check the size and pop N if there's too many.
    fn get_or_create_jar_provider(
        &self,
        segment: SnapshotSegment,
        block_range: &RangeInclusive<u64>,
    ) -> ProviderResult<SnapshotJarProvider<'_>> {
        let key = (*block_range.end(), segment);
        if let Some(jar) = self.map.get(&key) {
            Ok(jar.into())
        } else {
            let jar =
                NippyJar::load(&self.path.join(segment.filename(block_range))).map(|jar| {
                    if self.load_filters {
                        return jar.load_filters()
                    }
                    Ok(jar)
                })??;

            self.map.insert(key, LoadedJar::new(jar)?);
            Ok(self.map.get(&key).expect("qed").into())
        }
    }

    /// Gets a snapshot segment's block range and transaction range from the provider inner block
    /// index.
    fn get_segment_ranges_from_block(
        &self,
        segment: SnapshotSegment,
        block: u64,
    ) -> Option<RangeInclusive<BlockNumber>> {
        self.snapshots_max_block
            .read()
            .get(&segment)
            .iter()
            .filter_map(|max| {
                if **max >= block {
                    return Some(find_fixed_range(BLOCKS_PER_SNAPSHOT, block))
                }
                None
            })
            .next()
    }

    /// Gets a snapshot segment's block range and transaction range from the provider inner
    /// transaction index.
    fn get_segment_ranges_from_transaction(
        &self,
        segment: SnapshotSegment,
        tx: u64,
    ) -> Option<RangeInclusive<BlockNumber>> {
        let snapshots = self.snapshots_tx_index.read();
        let segment_snapshots = snapshots.get(&segment)?;

        let mut range = segment_snapshots.range(..tx);
        let (tx_end, block_range) = range.next_back()?;
        let tx_start = range.next_back().map(|(tx, _)| *tx).unwrap_or_default();
        let tx_range = tx_start..=*tx_end;

        tx_range.contains(&tx).then(|| block_range.clone())
    }

    /// Updates the inner transaction and block index
    pub fn update_index(&self) -> ProviderResult<()> {
        let mut max_block = self.snapshots_max_block.write();
        let mut tx_index = self.snapshots_tx_index.write();

        for (segment, ranges) in iter_snapshots(&self.path)? {
            // Update last block for each segment
            if let Some((block_range, _)) = ranges.last() {
                max_block.insert(segment, *block_range.end());
            }

            // Update tx -> block_range index
            for (block_range, tx_range) in ranges {
                if let Some(tx_range) = tx_range {
                    let tx_end = *tx_range.end();

                    match tx_index.entry(segment) {
                        Entry::Occupied(mut index) => {
                            index.get_mut().insert(tx_end, block_range);
                        }
                        Entry::Vacant(index) => {
                            index.insert(BTreeMap::from([(tx_end, block_range)]));
                        }
                    };
                }
            }
        }

        Ok(())
    }

    /// Gets the highest snapshot block if it exists for a snapshot segment.
    pub fn get_highest_snapshot_block(&self, segment: SnapshotSegment) -> Option<BlockNumber> {
        self.snapshots_max_block.read().get(&segment).map(|max_block| *max_block)
    }

    /// Gets the highest snapshotted transaction.
    pub fn get_highest_snapshot_tx(&self, segment: SnapshotSegment) -> Option<TxNumber> {
        self.snapshots_tx_index
            .read()
            .get(&segment)
            .and_then(|index| index.last_key_value().map(|(last_tx, _)| *last_tx))
    }

    /// Iterates through segment snapshots in reverse order, executing a function until it returns
    /// some object. Useful for finding objects by [`TxHash`] or [`BlockHash`].
    pub fn find_snapshot<T>(
        &self,
        segment: SnapshotSegment,
        func: impl Fn(SnapshotJarProvider<'_>) -> ProviderResult<Option<T>>,
    ) -> ProviderResult<Option<T>> {
        if let Some(highest_block) = self.get_highest_snapshot_block(segment) {
            let mut range = find_fixed_range(BLOCKS_PER_SNAPSHOT, highest_block);
            while *range.end() > 0 {
                if let Some(res) = func(self.get_or_create_jar_provider(segment, &range)?)? {
                    return Ok(Some(res))
                }
                range = range.start().saturating_sub(BLOCKS_PER_SNAPSHOT)..=
                    range.end().saturating_sub(BLOCKS_PER_SNAPSHOT);
            }
        }

        Ok(None)
    }
    /// Fetches data within a specified range across multiple snapshot files.
    ///
    /// This function iteratively retrieves data using `get_fn` for each item in the given range.
    /// It continues fetching until the end of the range is reached or the provided `predicate`
    /// returns false.
    pub fn fetch_range<T, F, P>(
        &self,
        segment: SnapshotSegment,
        range: Range<u64>,
        get_fn: F,
        mut predicate: P,
    ) -> ProviderResult<Vec<T>>
    where
        F: Fn(&mut SnapshotCursor<'_>, u64) -> ProviderResult<Option<T>>,
        P: FnMut(&T) -> bool,
    {
        let get_provider = |start: u64| match segment {
            SnapshotSegment::Headers => self.get_segment_provider_from_block(segment, start, None),
            SnapshotSegment::Transactions | SnapshotSegment::Receipts => {
                self.get_segment_provider_from_transaction(segment, start, None)
            }
        };

        let mut result = Vec::with_capacity((range.end - range.start).min(100) as usize);
        let mut provider = get_provider(range.start)?;
        let mut cursor = provider.cursor()?;

        // advances number in range
        'outer: for number in range {
            // advances snapshot files if `get_fn` returns None
            'inner: loop {
                match get_fn(&mut cursor, number)? {
                    Some(res) => {
                        if !predicate(&res) {
                            break 'outer
                        }
                        result.push(res);
                        break 'inner;
                    }
                    None => {
                        provider = get_provider(number)?;
                        cursor = provider.cursor()?;
                    }
                }
            }
        }

        Ok(result)
    }

    /// Returns directory where snapshots are located.
    pub fn directory(&self) -> &Path {
        &self.path
    }
}

/// Helper trait to manage different [`SnapshotProviderRW`] of an `Arc<SnapshotProvider`
pub trait SnapshotWriter {
    /// Returns a mutable reference to a [`SnapshotProviderRW`] of a [`SnapshotSegment`].
    fn writer(
        &self,
        block: BlockNumber,
        segment: SnapshotSegment,
    ) -> ProviderResult<RefMut<'_, SnapshotSegment, SnapshotProviderRW<'static>>>;

    /// Returns a mutable reference to a [`SnapshotProviderRW`] of the latest [`SnapshotSegment`].
    fn latest_writer(
        &self,
        segment: SnapshotSegment,
    ) -> ProviderResult<RefMut<'_, SnapshotSegment, SnapshotProviderRW<'static>>>;

    /// Commits all changes of all [`SnapshotProviderRW`] of all [`SnapshotSegment`].
    fn commit(&self) -> ProviderResult<()>;
}

impl SnapshotWriter for Arc<SnapshotProvider> {
    fn writer(
        &self,
        block: BlockNumber,
        segment: SnapshotSegment,
    ) -> ProviderResult<RefMut<'_, SnapshotSegment, SnapshotProviderRW<'static>>> {
        if let Some(writer) = self.writers.get_mut(&segment) {
            Ok(writer)
        } else {
            self.writers.insert(segment, SnapshotProviderRW::new(segment, block, self.clone())?);
            Ok(self.writers.get_mut(&segment).expect("qed").into())
        }
    }

    fn latest_writer(
        &self,
        segment: SnapshotSegment,
    ) -> ProviderResult<RefMut<'_, SnapshotSegment, SnapshotProviderRW<'static>>> {
        self.writer(self.get_highest_snapshot_block(segment).unwrap_or_default(), segment)
    }

    fn commit(&self) -> ProviderResult<()> {
        for mut writer in self.writers.iter_mut() {
            writer.commit()?;
        }
        Ok(())
    }
}

impl HeaderProvider for SnapshotProvider {
    fn header(&self, block_hash: &BlockHash) -> ProviderResult<Option<Header>> {
        self.find_snapshot(SnapshotSegment::Headers, |jar_provider| {
            Ok(jar_provider
                .cursor()?
                .get_two::<HeaderMask<Header, BlockHash>>(block_hash.into())?
                .and_then(|(header, hash)| {
                    if &hash == block_hash {
                        return Some(header)
                    }
                    None
                }))
        })
    }

    fn header_by_number(&self, num: BlockNumber) -> ProviderResult<Option<Header>> {
        self.get_segment_provider_from_block(SnapshotSegment::Headers, num, None)?
            .header_by_number(num)
    }

    fn header_td(&self, block_hash: &BlockHash) -> ProviderResult<Option<U256>> {
        self.find_snapshot(SnapshotSegment::Headers, |jar_provider| {
            Ok(jar_provider
                .cursor()?
                .get_two::<HeaderMask<CompactU256, BlockHash>>(block_hash.into())?
                .and_then(|(td, hash)| (&hash == block_hash).then_some(td.0)))
        })
    }

    fn header_td_by_number(&self, num: BlockNumber) -> ProviderResult<Option<U256>> {
        self.get_segment_provider_from_block(SnapshotSegment::Headers, num, None)?
            .header_td_by_number(num)
    }

    fn headers_range(&self, range: impl RangeBounds<BlockNumber>) -> ProviderResult<Vec<Header>> {
        self.fetch_range(
            SnapshotSegment::Headers,
            to_range(range),
            |cursor, number| cursor.get_one::<HeaderMask<Header>>(number.into()),
            |_| true,
        )
    }

    fn sealed_header(&self, num: BlockNumber) -> ProviderResult<Option<SealedHeader>> {
        self.get_segment_provider_from_block(SnapshotSegment::Headers, num, None)?
            .sealed_header(num)
    }

    fn sealed_headers_while(
        &self,
        range: impl RangeBounds<BlockNumber>,
        predicate: impl FnMut(&SealedHeader) -> bool,
    ) -> ProviderResult<Vec<SealedHeader>> {
        self.fetch_range(
            SnapshotSegment::Headers,
            to_range(range),
            |cursor, number| {
                Ok(cursor
                    .get_two::<HeaderMask<Header, BlockHash>>(number.into())?
                    .map(|(header, hash)| header.seal(hash)))
            },
            predicate,
        )
    }
}

impl BlockHashReader for SnapshotProvider {
    fn block_hash(&self, num: u64) -> ProviderResult<Option<B256>> {
        self.get_segment_provider_from_block(SnapshotSegment::Headers, num, None)?.block_hash(num)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.fetch_range(
            SnapshotSegment::Headers,
            start..end,
            |cursor, number| cursor.get_one::<HeaderMask<BlockHash>>(number.into()),
            |_| true,
        )
    }
}

impl ReceiptProvider for SnapshotProvider {
    fn receipt(&self, num: TxNumber) -> ProviderResult<Option<Receipt>> {
        self.get_segment_provider_from_transaction(SnapshotSegment::Receipts, num, None)?
            .receipt(num)
    }

    fn receipt_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Receipt>> {
        if let Some(num) = self.transaction_id(hash)? {
            return self.receipt(num)
        }
        Ok(None)
    }

    fn receipts_by_block(&self, _block: BlockHashOrNumber) -> ProviderResult<Option<Vec<Receipt>>> {
        unreachable!()
    }

    fn receipts_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Receipt>> {
        self.fetch_range(
            SnapshotSegment::Receipts,
            to_range(range),
            |cursor, number| cursor.get_one::<ReceiptMask<Receipt>>(number.into()),
            |_| true,
        )
    }
}

impl TransactionsProviderExt for SnapshotProvider {
    fn transaction_hashes_by_range(
        &self,
        tx_range: Range<TxNumber>,
    ) -> ProviderResult<Vec<(TxHash, TxNumber)>> {
        self.fetch_range(
            SnapshotSegment::Transactions,
            tx_range,
            |cursor, number| {
                let tx =
                    cursor.get_one::<TransactionMask<TransactionSignedNoHash>>(number.into())?;
                Ok(tx.map(|tx| (tx.hash(), cursor.number())))
            },
            |_| true,
        )
    }
}

impl TransactionsProvider for SnapshotProvider {
    fn transaction_id(&self, tx_hash: TxHash) -> ProviderResult<Option<TxNumber>> {
        self.find_snapshot(SnapshotSegment::Transactions, |jar_provider| {
            let mut cursor = jar_provider.cursor()?;
            if cursor
                .get_one::<TransactionMask<TransactionSignedNoHash>>((&tx_hash).into())?
                .and_then(|tx| (tx.hash() == tx_hash).then_some(tx))
                .is_some()
            {
                Ok(Some(cursor.number()))
            } else {
                Ok(None)
            }
        })
    }

    fn transaction_by_id(&self, num: TxNumber) -> ProviderResult<Option<TransactionSigned>> {
        self.get_segment_provider_from_transaction(SnapshotSegment::Transactions, num, None)?
            .transaction_by_id(num)
    }

    fn transaction_by_id_no_hash(
        &self,
        num: TxNumber,
    ) -> ProviderResult<Option<TransactionSignedNoHash>> {
        self.get_segment_provider_from_transaction(SnapshotSegment::Transactions, num, None)?
            .transaction_by_id_no_hash(num)
    }

    fn transaction_by_hash(&self, hash: TxHash) -> ProviderResult<Option<TransactionSigned>> {
        self.find_snapshot(SnapshotSegment::Transactions, |jar_provider| {
            Ok(jar_provider
                .cursor()?
                .get_one::<TransactionMask<TransactionSignedNoHash>>((&hash).into())?
                .map(|tx| tx.with_hash())
                .and_then(|tx| (tx.hash_ref() == &hash).then_some(tx)))
        })
    }

    fn transaction_by_hash_with_meta(
        &self,
        _hash: TxHash,
    ) -> ProviderResult<Option<(TransactionSigned, TransactionMeta)>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn transaction_block(&self, _id: TxNumber) -> ProviderResult<Option<BlockNumber>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn transactions_by_block(
        &self,
        _block_id: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<TransactionSigned>>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn transactions_by_block_range(
        &self,
        _range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<TransactionSigned>>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn senders_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        let txes = self.transactions_by_tx_range(range)?;
        TransactionSignedNoHash::recover_signers(&txes, txes.len())
            .ok_or(ProviderError::SenderRecoveryError)
    }

    fn transactions_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<reth_primitives::TransactionSignedNoHash>> {
        self.fetch_range(
            SnapshotSegment::Transactions,
            to_range(range),
            |cursor, number| {
                cursor.get_one::<TransactionMask<TransactionSignedNoHash>>(number.into())
            },
            |_| true,
        )
    }

    fn transaction_sender(&self, id: TxNumber) -> ProviderResult<Option<Address>> {
        Ok(self.transaction_by_id_no_hash(id)?.and_then(|tx| tx.recover_signer()))
    }
}

/* Cannot be successfully implemented but must exist for trait requirements */

impl BlockNumReader for SnapshotProvider {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn block_number(&self, _hash: B256) -> ProviderResult<Option<BlockNumber>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }
}

impl BlockReader for SnapshotProvider {
    fn find_block_by_hash(
        &self,
        _hash: B256,
        _source: BlockSource,
    ) -> ProviderResult<Option<Block>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn block(&self, _id: BlockHashOrNumber) -> ProviderResult<Option<Block>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn pending_block(&self) -> ProviderResult<Option<SealedBlock>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn pending_block_with_senders(&self) -> ProviderResult<Option<SealedBlockWithSenders>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn pending_block_and_receipts(&self) -> ProviderResult<Option<(SealedBlock, Vec<Receipt>)>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn ommers(&self, _id: BlockHashOrNumber) -> ProviderResult<Option<Vec<Header>>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn block_body_indices(&self, _num: u64) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn block_with_senders(
        &self,
        _id: BlockHashOrNumber,
        _transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<BlockWithSenders>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn block_range(&self, _range: RangeInclusive<BlockNumber>) -> ProviderResult<Vec<Block>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }
}

impl WithdrawalsProvider for SnapshotProvider {
    fn withdrawals_by_block(
        &self,
        _id: BlockHashOrNumber,
        _timestamp: u64,
    ) -> ProviderResult<Option<Vec<Withdrawal>>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }

    fn latest_withdrawal(&self) -> ProviderResult<Option<Withdrawal>> {
        // Required data not present in snapshots
        Err(ProviderError::UnsupportedProvider)
    }
}
