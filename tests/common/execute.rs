use alloy_evm::{EthEvm, Evm, precompiles::PrecompilesMap};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};

use grevm::{ParallelState, ParallelTakeBundle, Scheduler};
use revm::{
    Context, DatabaseCommit, DatabaseRef, MainBuilder, MainContext, handler::EthPrecompiles,
};
use revm_context::{
    BlockEnv, CfgEnv, TxEnv,
    result::{EVMError, ExecutionResult, ResultAndState},
};
use revm_database::{
    AccountRevert, BundleAccount, BundleState, PlainAccount, StateBuilder,
    states::{StorageSlot, bundle_state::BundleRetention},
};
use revm_inspector::NoOpInspector;
use revm_primitives::{
    Address, KECCAK_EMPTY, U256, alloy_primitives::U160, hardfork::SpecId, uint,
};
use revm_state::AccountInfo;

use std::{
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    sync::Arc,
    time::Instant,
};

use crate::common::MINER_ADDRESS;

pub(crate) fn compare_result_and_state(left: &Vec<ResultAndState>, right: &Vec<ResultAndState>) {
    assert_eq!(left.len(), right.len());
    for (l, r) in left.iter().zip(right.iter()) {}
    for txid in 0..left.len() {
        assert_eq!(left[txid].lazy_reward, right[txid].lazy_reward, "Tx {}", txid);
        assert_eq!(left[txid].result, right[txid].result, "Tx {}", txid);
        let l = &left[txid].state;
        let r = &right[txid].state;
        for (address, l) in l {
            let r =
                r.get(address).expect(format!("no account {:?} in Tx {}", address, txid).as_str());
            assert_eq!(l.info, r.info, "account {:?} info in Tx {}", address, txid);
            assert_eq!(l.storage, r.storage, "account {:?} storage in Tx {}", address, txid);
            assert_eq!(l.status, r.status, "account {:?} status in Tx {}", address, txid);
        }
    }
}

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
    disable_nonce_check: bool,
    parallel_metrics: HashMap<&str, usize>,
) where
    DB: DatabaseRef + Send + Sync + Debug,
    DB::Error: Send + Sync + Clone + Debug + 'static,
{
    // create registry for metrics
    let recorder = DebuggingRecorder::new();

    let mut env = BlockEnv::default();
    let mut cfg = CfgEnv::new_with_spec(SpecId::SHANGHAI);
    cfg.disable_nonce_check = disable_nonce_check;
    env.beneficiary = Address::from(U160::from(MINER_ADDRESS));
    let db = Arc::new(db);
    let txs = Arc::new(txs);

    let parallel_result = metrics::with_local_recorder(&recorder, || {
        let start = Instant::now();
        let state = ParallelState::new(db.clone(), true, true);
        let mut executor = Scheduler::new(cfg.clone(), env.clone(), txs.clone(), state, with_hints);
        // set determined partitions
        executor.parallel_execute(Some(23)).expect("parallel execute failed");
        println!("Grevm parallel execute time: {}ms", start.elapsed().as_millis());

        let snapshot = recorder.snapshotter().snapshot();
        for (key, _, _, value) in snapshot.into_vec() {
            let value = match value {
                DebugValue::Counter(v) => v as usize,
                DebugValue::Gauge(v) => v.0 as usize,
                DebugValue::Histogram(v) => v.last().cloned().map_or(0, |ov| ov.0 as usize),
            };
            println!("metrics: {} => value: {:?}", key.key().name(), value);
            if let Some(metric) = parallel_metrics.get(key.key().name()) {
                assert_eq!(*metric, value);
            }
        }
        let (results, mut state) = executor.take_result_and_state();
        (results, state.parallel_take_bundle(BundleRetention::Reverts))
    });

    let start = Instant::now();
    let reth_result = execute_revm_sequential(db.clone(), cfg.clone(), env.clone(), &*txs).unwrap();
    println!("Origin sequential execute time: {}ms", start.elapsed().as_millis());

    let mut max_gas_spent = 0;
    let mut max_gas_used = 0;
    for result in reth_result.0.iter() {
        match result {
            ExecutionResult::Success { gas_used, gas_refunded, .. } => {
                max_gas_spent = max_gas_spent.max(gas_used + gas_refunded);
                max_gas_used = max_gas_used.max(*gas_used);
            }
            _ => panic!("result is not success"),
        }
    }
    println!("max_gas_spent: {}, max_gas_used: {}", max_gas_spent, max_gas_used);
    compare_execution_result(&reth_result.0, &parallel_result.0);
    compare_bundle_state(&reth_result.1, &parallel_result.1);
}

/// Simulate the sequential execution of transactions in reth
pub(crate) fn execute_revm_sequential<DB>(
    db: DB,
    cfg: CfgEnv,
    env: BlockEnv,
    txs: &[TxEnv],
) -> Result<(Vec<ExecutionResult>, BundleState), EVMError<DB::Error>>
where
    DB: DatabaseRef + Debug,
    DB::Error: Send + Sync + Debug + 'static,
{
    let db: revm_database::State<revm_database::WrapDatabaseRef<DB>> =
        StateBuilder::new().with_bundle_update().with_database_ref(db).build();
    let evm = Context::mainnet()
        .with_db(db)
        .with_cfg(cfg)
        .with_block(env)
        .build_mainnet_with_inspector(NoOpInspector {})
        .with_precompiles(PrecompilesMap::from_static(EthPrecompiles::default().precompiles));
    let mut evm = EthEvm::new(evm, false);

    let mut results = Vec::with_capacity(txs.len());
    for tx in txs {
        let result_and_state = evm.transact_raw(tx.clone())?;
        evm.db_mut().commit(result_and_state.state);
        results.push(result_and_state.result);
    }
    evm.db_mut().merge_transitions(BundleRetention::Reverts);

    Ok((results, evm.db_mut().take_bundle()))
}
