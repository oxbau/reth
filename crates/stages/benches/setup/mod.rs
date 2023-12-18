use itertools::concat;
use reth_db::{
    cursor::DbCursorRO,
    tables,
    test_utils::TempDatabase,
    transaction::{DbTx, DbTxMut},
    DatabaseEnv,
};
use reth_interfaces::test_utils::{
    generators,
    generators::{
        random_block_range, random_changeset_range, random_contract_account_range,
        random_eoa_account_range,
    },
};
use reth_primitives::{fs, Account, Address, SealedBlock, B256, U256};
use reth_stages::{
    stages::{AccountHashingStage, StorageHashingStage},
    test_utils::TestStageDB,
    ExecInput, Stage, UnwindInput,
};
use reth_trie::StateRoot;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

mod constants;

mod account_hashing;
pub use account_hashing::*;

pub(crate) type StageRange = (ExecInput, UnwindInput);

pub(crate) fn stage_unwind<S: Clone + Stage<Arc<TempDatabase<DatabaseEnv>>>>(
    stage: S,
    db: &TestStageDB,
    range: StageRange,
) {
    let (_, unwind) = range;

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let mut stage = stage.clone();
        let provider = db.factory.provider_rw().unwrap();

        // Clear previous run
        stage
            .unwind(&provider, unwind)
            .map_err(|e| {
                format!(
                    "{e}\nMake sure your test database at `{}` isn't too old and incompatible with newer stage changes.",
                    db.factory.db_ref().path().display()
                )
            })
            .unwrap();

        provider.commit().unwrap();
    });
}

pub(crate) fn unwind_hashes<S: Clone + Stage<Arc<TempDatabase<DatabaseEnv>>>>(
    stage: S,
    db: &TestStageDB,
    range: StageRange,
) {
    let (input, unwind) = range;

    let mut stage = stage.clone();
    let provider = db.factory.provider_rw().unwrap();

    StorageHashingStage::default().unwind(&provider, unwind).unwrap();
    AccountHashingStage::default().unwind(&provider, unwind).unwrap();

    // Clear previous run
    stage.unwind(&provider, unwind).unwrap();

    AccountHashingStage::default().execute(&provider, input).unwrap();
    StorageHashingStage::default().execute(&provider, input).unwrap();

    provider.commit().unwrap();
}

// Helper for generating testdata for the benchmarks.
// Returns the path to the database file.
pub(crate) fn txs_testdata(num_blocks: u64) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("txs-bench");
    let txs_range = 100..150;

    // number of storage changes per transition
    let n_changes = 0..3;

    // range of possible values for a storage key
    let key_range = 0..300;

    // number of accounts
    let n_eoa = 131;
    let n_contract = 31;

    // rng
    let mut rng = generators::rng();

    if !path.exists() {
        // create the dirs
        fs::create_dir_all(&path).unwrap();
        println!("Transactions testdata not found, generating to {:?}", path.display());
        let db = TestStageDB::new(&path);

        let accounts: BTreeMap<Address, Account> = concat([
            random_eoa_account_range(&mut rng, 0..n_eoa),
            random_contract_account_range(&mut rng, &mut (0..n_contract)),
        ])
        .into_iter()
        .collect();

        let mut blocks = random_block_range(&mut rng, 0..=num_blocks, B256::ZERO, txs_range);

        let (transitions, start_state) = random_changeset_range(
            &mut rng,
            blocks.iter().take(2),
            accounts.into_iter().map(|(addr, acc)| (addr, (acc, Vec::new()))),
            n_changes.clone(),
            key_range.clone(),
        );

        db.insert_accounts_and_storages(start_state.clone()).unwrap();

        // make first block after genesis have valid state root
        let (root, updates) =
            StateRoot::new(db.factory.provider_rw().unwrap().tx_ref()).root_with_updates().unwrap();
        let second_block = blocks.get_mut(1).unwrap();
        let cloned_second = second_block.clone();
        let mut updated_header = cloned_second.header.unseal();
        updated_header.state_root = root;
        *second_block = SealedBlock { header: updated_header.seal_slow(), ..cloned_second };

        let offset = transitions.len() as u64;

        db.insert_changesets(transitions, None).unwrap();
        db.commit(|tx| Ok(updates.flush(tx)?)).unwrap();

        let (transitions, final_state) = random_changeset_range(
            &mut rng,
            blocks.iter().skip(2),
            start_state,
            n_changes,
            key_range,
        );

        db.insert_changesets(transitions, Some(offset)).unwrap();

        db.insert_accounts_and_storages(final_state).unwrap();

        // make last block have valid state root
        let root = {
            let tx_mut = db.factory.provider_rw().unwrap();
            let root = StateRoot::new(tx_mut.tx_ref()).root().unwrap();
            tx_mut.commit().unwrap();
            root
        };

        let last_block = blocks.last_mut().unwrap();
        let cloned_last = last_block.clone();
        let mut updated_header = cloned_last.header.unseal();
        updated_header.state_root = root;
        *last_block = SealedBlock { header: updated_header.seal_slow(), ..cloned_last };

        db.insert_blocks(blocks.iter(), None).unwrap();

        // initialize TD
        db.commit(|tx| {
            let (head, _) = tx.cursor_read::<tables::Headers>()?.first()?.unwrap_or_default();
            Ok(tx.put::<tables::HeaderTD>(head, U256::from(0).into())?)
        })
        .unwrap();
    }

    path
}
