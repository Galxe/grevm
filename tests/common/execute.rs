use crate::common::{storage::InMemoryDB, MINER_ADDRESS};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};

use alloy_chains::NamedChain;
use grevm::{ExecuteOutput, GrevmScheduler};
use revm::{
    db::{
        states::{bundle_state::BundleRetention, StorageSlot},
        AccountRevert, BundleAccount, BundleState, PlainAccount,
    },
    primitives::{
        alloy_primitives::U160, uint, AccountInfo, Address, Bytecode, EVMError, Env,
        ExecutionResult, SpecId, TxEnv, B256, KECCAK_EMPTY, U256,
    },
    DatabaseCommit, DatabaseRef, EvmBuilder, StateBuilder,
};
use revm_primitives::EnvWithHandlerCfg;
use std::{
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    fs::{self, File},
    io::BufReader,
    sync::Arc,
    time::Instant,
};

pub(crate) fn compare_bundle_state(left: &BundleState, right: &BundleState) {
    assert!(
        left.contracts.keys().all(|k| right.contracts.contains_key(k)),
        "Left contracts: {:?}, Right contracts: {:?}",
        left.contracts.keys(),
        right.contracts.keys()
    );
    assert_eq!(
        left.contracts.len(),
        right.contracts.len(),
        "Left contracts: {:?}, Right contracts: {:?}",
        left.contracts.keys(),
        right.contracts.keys()
    );

    let left_state: BTreeMap<&Address, &BundleAccount> = left.state.iter().collect();
    let right_state: BTreeMap<&Address, &BundleAccount> = right.state.iter().collect();
    assert_eq!(left_state.len(), right_state.len());

    for ((addr1, account1), (addr2, account2)) in
        left_state.into_iter().zip(right_state.into_iter())
    {
        assert_eq!(addr1, addr2);
        assert_eq!(account1.info, account2.info, "Address: {:?}", addr1);
        assert_eq!(account1.original_info, account2.original_info, "Address: {:?}", addr1);
        assert_eq!(account1.status, account2.status, "Address: {:?}", addr1);
        assert_eq!(account1.storage.len(), account2.storage.len());
        let left_storage: BTreeMap<&U256, &StorageSlot> = account1.storage.iter().collect();
        let right_storage: BTreeMap<&U256, &StorageSlot> = account2.storage.iter().collect();
        for (s1, s2) in left_storage.into_iter().zip(right_storage.into_iter()) {
            assert_eq!(s1, s2, "Address: {:?}", addr1);
        }
    }

    assert_eq!(left.reverts.len(), right.reverts.len());
    for (left, right) in left.reverts.iter().zip(right.reverts.iter()) {
        assert_eq!(left.len(), right.len());
        let right: HashMap<&Address, &AccountRevert> = right.iter().map(|(k, v)| (k, v)).collect();
        for (addr, revert) in left.iter() {
            assert_eq!(revert, *right.get(addr).unwrap(), "Address: {:?}", addr);
        }
    }
}

pub(crate) fn compare_execution_result(left: &Vec<ExecutionResult>, right: &Vec<ExecutionResult>) {
    for (i, (left_res, right_res)) in left.iter().zip(right.iter()).enumerate() {
        assert_eq!(left_res, right_res, "Tx {}", i);
    }
    assert_eq!(left.len(), right.len());
}

pub(crate) fn mock_miner_account() -> (Address, PlainAccount) {
    let address = Address::from(U160::from(MINER_ADDRESS));
    let account = PlainAccount {
        info: AccountInfo { balance: U256::from(0), nonce: 1, code_hash: KECCAK_EMPTY, code: None },
        storage: Default::default(),
    };
    (address, account)
}

pub(crate) fn mock_eoa_account(idx: usize) -> (Address, PlainAccount) {
    let address = Address::from(U160::from(idx));
    let account = PlainAccount {
        info: AccountInfo {
            balance: uint!(1_000_000_000_000_000_000_U256),
            nonce: 1,
            code_hash: KECCAK_EMPTY,
            code: None,
        },
        storage: Default::default(),
    };
    (address, account)
}

pub(crate) fn mock_block_accounts(from: usize, size: usize) -> HashMap<Address, PlainAccount> {
    let mut accounts: HashMap<Address, PlainAccount> =
        (from..(from + size)).map(mock_eoa_account).collect();
    let miner = mock_miner_account();
    accounts.insert(miner.0, miner.1);
    accounts
}

pub(crate) fn compare_evm_execute<DB>(
    db: DB,
    txs: Vec<TxEnv>,
    with_hints: bool,
    parallel_metrics: HashMap<&str, usize>,
) where
    DB: DatabaseRef + Send + Sync + 'static,
    DB::Error: Send + Sync + Clone + Debug,
{
    // create registry for metrics
    let recorder = DebuggingRecorder::new();

    let mut env = Env::default();
    env.cfg.chain_id = NamedChain::Mainnet.into();
    env.block.coinbase = Address::from(U160::from(MINER_ADDRESS));
    let db = Arc::new(db);
    let txs = Arc::new(txs);
    let start = Instant::now();
    let sequential_result = {
        let mut executor =
            GrevmScheduler::new(SpecId::LATEST, env.clone(), db.clone(), txs.clone(), None);
        let sequential_result = executor.force_sequential_execute().unwrap();
        println!("Grevm sequential execute time: {}ms", start.elapsed().as_millis());
        let database = Arc::get_mut(&mut executor.database).unwrap();
        database.state.merge_transitions(BundleRetention::Reverts);
        (sequential_result, database.state.take_bundle())
    };

    let parallel_result = metrics::with_local_recorder(&recorder, || {
        let start = Instant::now();
        let mut executor =
            GrevmScheduler::new(SpecId::LATEST, env.clone(), db.clone(), txs.clone(), None);
        // set determined partitions
        let parallel_result = executor.force_parallel_execute(with_hints, Some(23)).unwrap();
        println!("Grevm parallel execute time: {}ms", start.elapsed().as_millis());

        let snapshot = recorder.snapshotter().snapshot();
        for (key, _, _, value) in snapshot.into_vec() {
            println!("metrics: {} => value: {:?}", key.key().name(), value);
            if let Some(metric) = parallel_metrics.get(key.key().name()) {
                let v = match value {
                    DebugValue::Counter(v) => v as usize,
                    DebugValue::Gauge(v) => v.0 as usize,
                    DebugValue::Histogram(v) => v.last().cloned().map_or(0, |ov| ov.0 as usize),
                };
                assert_eq!(*metric, v);
            }
        }
        let database = Arc::get_mut(&mut executor.database).unwrap();
        database.state.merge_transitions(BundleRetention::Reverts);
        (parallel_result, database.state.take_bundle())
    });

    let start = Instant::now();
    let reth_result = execute_revm_sequential(db.clone(), SpecId::LATEST, env.clone(), &*txs);
    println!("Origin sequential execute time: {}ms", start.elapsed().as_millis());

    let mut max_gas_spent = 0;
    let mut max_gas_used = 0;
    for result in &reth_result.as_ref().unwrap().0.results {
        match result {
            ExecutionResult::Success { gas_used, gas_refunded, .. } => {
                max_gas_spent = max_gas_spent.max(gas_used + gas_refunded);
                max_gas_used = max_gas_used.max(*gas_used);
            }
            _ => panic!("result is not success"),
        }
    }
    println!("max_gas_spent: {}, max_gas_used: {}", max_gas_spent, max_gas_used);

    compare_execution_result(
        &reth_result.as_ref().unwrap().0.results,
        &sequential_result.0.results,
    );
    compare_execution_result(&reth_result.as_ref().unwrap().0.results, &parallel_result.0.results);

    compare_bundle_state(&reth_result.as_ref().unwrap().1, &sequential_result.1);
    compare_bundle_state(&reth_result.as_ref().unwrap().1, &parallel_result.1);
}

/// Simulate the sequential execution of transactions in reth
pub(crate) fn execute_revm_sequential<DB>(
    db: DB,
    spec_id: SpecId,
    env: Env,
    txs: &[TxEnv],
) -> Result<(ExecuteOutput, BundleState), EVMError<DB::Error>>
where
    DB: DatabaseRef,
    DB::Error: Debug,
{
    let db = StateBuilder::new().with_bundle_update().with_database_ref(db).build();
    let mut evm =
        EvmBuilder::default().with_db(db).with_spec_id(spec_id).with_env(Box::new(env)).build();

    let mut results = Vec::with_capacity(txs.len());
    for tx in txs {
        *evm.tx_mut() = tx.clone();
        let result_and_state = evm.transact()?;
        evm.db_mut().commit(result_and_state.state);
        results.push(result_and_state.result);
    }

    evm.db_mut().merge_transitions(BundleRetention::Reverts);

    Ok((ExecuteOutput { results }, evm.db_mut().take_bundle()))
}

const TEST_DATA_DIR: &str = "test_data";

pub(crate) fn load_bytecodes_from_disk() -> HashMap<B256, Bytecode> {
    // Parse bytecodes
    bincode::deserialize_from(BufReader::new(
        File::open(format!("{TEST_DATA_DIR}/bytecodes.bincode")).unwrap(),
    ))
    .unwrap()
}

pub(crate) fn load_block_from_disk(
    block_number: u64,
) -> (EnvWithHandlerCfg, Vec<TxEnv>, InMemoryDB) {
    // Parse block
    let env: EnvWithHandlerCfg = serde_json::from_reader(BufReader::new(
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/env.json")).unwrap(),
    ))
    .unwrap();
    let txs: Vec<TxEnv> = serde_json::from_reader(BufReader::new(
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/txs.json")).unwrap(),
    ))
    .unwrap();

    // Parse state
    let accounts: HashMap<Address, PlainAccount> = serde_json::from_reader(BufReader::new(
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/pre_state.json")).unwrap(),
    ))
    .unwrap();

    // Parse block hashes
    let block_hashes: HashMap<u64, B256> =
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/block_hashes.json"))
            .map(|file| serde_json::from_reader(BufReader::new(file)).unwrap())
            .unwrap_or_default();

    // Parse bytecodes
    let bytecodes: HashMap<B256, Bytecode> =
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/bytecodes.bincode"))
            .map(|file| bincode::deserialize_from(BufReader::new(file)).unwrap())
            .unwrap_or_default();

    (env, txs, InMemoryDB::new(accounts, bytecodes, block_hashes))
}

pub(crate) fn for_each_block_from_disk(
    mut handler: impl FnMut(EnvWithHandlerCfg, Vec<TxEnv>, InMemoryDB),
) {
    // Parse bytecodes
    let bytecodes = load_bytecodes_from_disk();

    for block_path in fs::read_dir(format!("{TEST_DATA_DIR}/blocks")).unwrap() {
        let block_path = block_path.unwrap().path();
        let block_number = block_path.file_name().unwrap().to_str().unwrap();

        let (env, txs, mut db) = load_block_from_disk(block_number.parse().unwrap());
        if db.bytecodes.is_empty() {
            // Use the global bytecodes if the block doesn't have its own
            db.bytecodes = bytecodes.clone();
        }
        handler(env, txs, db);
    }
}
