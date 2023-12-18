//! Support for snapshotting.

use crate::{segments, segments::Segment, SnapshotterError};
use reth_db::{
    cursor::DbCursorRO, database::Database, snapshot::iter_snapshots, tables, transaction::DbTx,
    Tables,
};
use reth_interfaces::RethResult;
use reth_primitives::{snapshot::HighestSnapshots, BlockNumber, SnapshotSegment, TxNumber};
use reth_provider::{
    providers::{SnapshotProvider, SnapshotWriter},
    BlockReader, DatabaseProviderRO, ProviderFactory, ReceiptProvider, TransactionsProvider,
    TransactionsProviderExt,
};
use std::{
    collections::HashMap,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::watch;
use tracing::{warn, Value};

/// Result of [Snapshotter::run] execution.
pub type SnapshotterResult = Result<SnapshotTargets, SnapshotterError>;

/// The snapshotter type itself with the result of [Snapshotter::run]
pub type SnapshotterWithResult<DB> = (Snapshotter<DB>, SnapshotterResult);

/// Snapshotting routine. Main snapshotting logic happens in [Snapshotter::run].
#[derive(Debug)]
pub struct Snapshotter<DB> {
    /// Provider factory
    provider_factory: ProviderFactory<DB>,
    /// Snapshot provider
    snapshot_provider: Arc<SnapshotProvider>,
}

/// Snapshot targets, per data part, measured in [`BlockNumber`].
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SnapshotTargets {
    headers: Option<RangeInclusive<BlockNumber>>,
    receipts: Option<RangeInclusive<BlockNumber>>,
    transactions: Option<RangeInclusive<BlockNumber>>,
}

impl SnapshotTargets {
    /// Returns `true` if any of the targets are [Some].
    pub fn any(&self) -> bool {
        self.headers.is_some() || self.receipts.is_some() || self.transactions.is_some()
    }

    // Returns `true` if all targets are either [`None`] or has beginning of the range equal to the
    // highest snapshot.
    fn is_contiguous_to_highest_snapshots(&self, snapshots: HighestSnapshots) -> bool {
        [
            (self.headers.as_ref(), snapshots.headers),
            (self.receipts.as_ref(), snapshots.receipts),
            (self.transactions.as_ref(), snapshots.transactions),
        ]
        .iter()
        .all(|(target, highest)| {
            target.map_or(true, |block_number| {
                highest.map_or(*block_number.start() == 0, |previous_block_number| {
                    *block_number.start() == previous_block_number + 1
                })
            })
        })
    }
}

impl<DB: Database> Snapshotter<DB> {
    /// Creates a new [Snapshotter].
    pub fn new(
        provider_factory: ProviderFactory<DB>,
        snapshot_provider: Arc<SnapshotProvider>,
    ) -> Self {
        Self { provider_factory, snapshot_provider }
    }

    /// Run the snapshotter
    pub fn run(&mut self, targets: SnapshotTargets) -> SnapshotterResult {
        let provider = self.provider_factory.provider()?;
        let snapshot_provider = &self.snapshot_provider;

        debug_assert!(
            targets.is_contiguous_to_highest_snapshots(snapshot_provider.get_highest_snapshots())
        );

        if let Some(block_range) = targets.transactions.clone() {
            let mut snapshot_writer =
                snapshot_provider.writer(*block_range.start(), SnapshotSegment::Transactions)?;

            let mut transactions_cursor =
                provider.tx_ref().cursor_read::<tables::Transactions>()?;

            for block in block_range {
                let Some(block_body_indices) = provider.block_body_indices(block)? else {
                    continue
                };
                let tx_range = block_body_indices.tx_num_range();
                let tx_walker = transactions_cursor.walk_range(tx_range)?;

                for entry in tx_walker {
                    let (tx_number, transaction) = entry?;

                    snapshot_writer.append_transaction(block, tx_number, transaction)?;
                }
            }
        }

        // TODO(alexey): snapshot headers and receipts

        snapshot_provider.commit()?;
        snapshot_provider.update_index()?;

        Ok(targets)
    }

    /// Returns a snapshot targets at the provided finalized block number.
    /// The target is determined by the check against highest snapshots.
    pub fn get_snapshot_targets(
        &self,
        finalized_block_number: BlockNumber,
    ) -> RethResult<SnapshotTargets> {
        let highest_snapshots = self.snapshot_provider.get_highest_snapshots();

        // Calculate block ranges to snapshot
        let headers = highest_snapshots.headers.unwrap_or_default()..=finalized_block_number;
        let receipts = highest_snapshots.receipts.unwrap_or_default()..=finalized_block_number;
        let transactions =
            highest_snapshots.transactions.unwrap_or_default()..=finalized_block_number;

        Ok(SnapshotTargets {
            headers: (!headers.is_empty()).then_some(headers),
            receipts: (!receipts.is_empty()).then_some(receipts),
            transactions: (!transactions.is_empty()).then_some(transactions),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{snapshotter::SnapshotTargets, Snapshotter};
    use assert_matches::assert_matches;
    use reth_interfaces::{
        test_utils::{generators, generators::random_block_range},
        RethError,
    };
    use reth_primitives::{snapshot::HighestSnapshots, B256};
    use reth_stages::test_utils::TestStageDB;

    #[test]
    fn run() {
        let mut rng = generators::rng();

        let db = TestStageDB::default();

        let blocks = random_block_range(&mut rng, 1..=3, B256::ZERO, 2..3);
        db.insert_blocks(blocks.iter(), Some(1)).expect("insert blocks");

        let snapshots_dir = tempfile::TempDir::new().unwrap();
        let provider_factory = db
            .factory
            .with_snapshots(snapshots_dir.path().to_path_buf())
            .expect("factory with snapshots");
        let snapshot_provider = provider_factory.snapshot_provider.clone().unwrap();

        let mut snapshotter = Snapshotter::new(provider_factory, snapshot_provider.clone());

        // Snapshot targets has data per part up to the passed finalized block number
        let targets = snapshotter.get_snapshot_targets(1).expect("get snapshot targets");
        assert_eq!(
            targets,
            SnapshotTargets {
                headers: Some(0..=1),
                receipts: Some(0..=1),
                transactions: Some(0..=1)
            }
        );

        snapshotter.run(targets).expect("run snapshotter");
        assert_eq!(
            snapshot_provider.get_highest_snapshots(),
            HighestSnapshots { headers: Some(1), receipts: None, transactions: None }
        );

        // Snapshot targets has data per part up to the passed finalized block number
        let targets = snapshotter.get_snapshot_targets(5).expect("get snapshot targets");
        assert_eq!(
            targets,
            SnapshotTargets {
                headers: Some(2..=3),
                receipts: Some(1..=3),
                transactions: Some(1..=3)
            }
        );

        snapshotter.run(targets).expect("run snapshotter");
        assert_eq!(
            snapshot_provider.get_highest_snapshots(),
            HighestSnapshots { headers: Some(3), receipts: None, transactions: None }
        );
    }
}
