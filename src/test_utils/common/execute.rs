use alloy_evm::{EthEvm, Evm, precompiles::PrecompilesMap};
use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};

use crate::{
    DelegatedSafetyConfig, GrevmConfig, ParallelState, ParallelTakeBundle, Scheduler,
    TxExecutionOutcome,
};
use revm::{
    Context, DatabaseCommit, DatabaseRef, MainBuilder, MainContext, handler::EthPrecompiles,
};
use revm_context::{
    BlockEnv, CfgEnv, TxEnv,
    result::{EVMError, ExecutionResult},
};
use revm_database::{
    AccountRevert, BundleAccount, BundleState, StateBuilder,
    states::{StorageSlot, bundle_state::BundleRetention},
};
use revm_inspector::NoOpInspector;
use revm_primitives::{Address, HashMap, U256, hardfork::SpecId};

use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

/// Lazily install a **process-wide** debugging recorder (once) and return its snapshotter.
///
/// We deliberately avoid a fresh per-call `metrics::with_local_recorder`: that recorder is
/// thread-local and scoped to one call, so grevm's worker-thread metrics and every block after the
/// first are silently dropped (only the first block printed a full set). One stable global recorder
/// instead captures metrics from all threads and all blocks; [`Snapshotter::snapshot`] drains the
/// histograms, so each call returns just that block's values and memory stays bounded.
///
/// Returns `None` if another global recorder is already installed.
fn metrics_snapshotter() -> Option<&'static Snapshotter> {
    static SNAPSHOTTER: OnceLock<Option<Snapshotter>> = OnceLock::new();
    SNAPSHOTTER
        .get_or_init(|| {
            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();
            recorder.install().ok().map(|()| snapshotter)
        })
        .as_ref()
}

fn assert_expected_metrics(
    observed: &BTreeMap<&'static str, usize>,
    expected: HashMap<&str, usize>,
) {
    for (name, expected) in expected {
        let actual = observed
            .get(name)
            .unwrap_or_else(|| panic!("expected metric `{name}` was not recorded"));
        assert_eq!(actual, &expected, "metric `{name}` mismatch");
    }
}

/// Runtime configuration for revm-compatibility tests.
///
/// These tests use upstream revm as their oracle, so grevm-local delegated-account policies must
/// stay disabled even if their defaults change later. Other scheduler settings still come from
/// the environment for compatibility with the existing test and benchmark controls.
pub fn revm_compatibility_config() -> GrevmConfig {
    GrevmConfig::from_env().with_delegated_safety(DelegatedSafetyConfig::disabled())
}

pub fn compare_bundle_state(left: &BundleState, right: &BundleState) {
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

    for ((addr1, account1), (addr2, account2)) in left_state.into_iter().zip(right_state) {
        assert_eq!(addr1, addr2);
        assert_eq!(account1.info, account2.info, "Address: {:?}", addr1);
        assert_eq!(account1.original_info, account2.original_info, "Address: {:?}", addr1);
        assert_eq!(account1.status, account2.status, "Address: {:?}", addr1);
        assert_eq!(account1.storage.len(), account2.storage.len());
        let left_storage: BTreeMap<&U256, &StorageSlot> = account1.storage.iter().collect();
        let right_storage: BTreeMap<&U256, &StorageSlot> = account2.storage.iter().collect();
        for (s1, s2) in left_storage.into_iter().zip(right_storage) {
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

    // The cached size accounting must match the sequential revm path. The parallel bundle path
    // computes these independently, so a divergence means it miscounted (e.g. recording reverts
    // without incrementing `reverts_size`).
    assert_eq!(left.state_size, right.state_size, "state_size mismatch");
    assert_eq!(left.reverts_size, right.reverts_size, "reverts_size mismatch");
}

pub fn compare_execution_result(left: &[ExecutionResult], right: &[TxExecutionOutcome]) {
    for (i, (left_res, right_res)) in left.iter().zip(right.iter()).enumerate() {
        let TxExecutionOutcome::Executed(right_res) = right_res else {
            panic!("valid reference transaction {i} was unexpectedly skipped: {right_res:?}");
        };
        assert_eq!(left_res, right_res, "Tx {}", i);
    }
    assert_eq!(left.len(), right.len());
}

pub fn compare_evm_execute<DB>(
    db: DB,
    txs: Vec<TxEnv>,
    disable_nonce_check: bool,
    parallel_metrics: HashMap<&str, usize>,
) where
    DB: DatabaseRef + Send + Sync + Debug,
    DB::Error: Send + Sync + Clone + Debug + 'static,
{
    compare_evm_execute_with_spec(db, txs, disable_nonce_check, parallel_metrics, SpecId::SHANGHAI);
}

/// Same as [`compare_evm_execute`] but lets the caller pick the [`SpecId`]. Needed for
/// fork-gated features such as EIP-7702 (active from [`SpecId::PRAGUE`]).
pub fn compare_evm_execute_with_spec<DB>(
    db: DB,
    txs: Vec<TxEnv>,
    disable_nonce_check: bool,
    parallel_metrics: HashMap<&str, usize>,
    spec: SpecId,
) where
    DB: DatabaseRef + Send + Sync + Debug,
    DB::Error: Send + Sync + Clone + Debug + 'static,
{
    let env = BlockEnv { beneficiary: super::account::MINER_ADDRESS, ..Default::default() };
    let mut cfg = CfgEnv::new_with_spec(spec);
    cfg.disable_nonce_check = disable_nonce_check;
    compare_evm_execute_with_env(db, txs, cfg, env, parallel_metrics);
}

/// Compare Grevm against an independent, strictly ordered revm execution that skips every
/// transaction-validation error.
///
/// Both per-transaction outcomes and the complete post-block bundle state are compared. This is
/// the reference path for testing Grevm's sequential fallback with blocks that intentionally
/// contain invalid transactions.
pub fn compare_evm_execute_skipping_invalid_with_spec<DB>(
    db: DB,
    txs: Vec<TxEnv>,
    spec: SpecId,
) -> Vec<TxExecutionOutcome>
where
    DB: DatabaseRef + Send + Sync + Debug,
    DB::Error: Send + Sync + Clone + Debug + 'static,
{
    let db = Arc::new(db);
    let txs = Arc::new(txs);
    let cfg = CfgEnv::new_with_spec(spec);
    let env = BlockEnv { beneficiary: super::account::MINER_ADDRESS, ..Default::default() };

    let (expected_outcomes, expected_bundle) =
        execute_revm_sequential_skipping_invalid(db.clone(), cfg.clone(), env.clone(), &txs)
            .unwrap_or_else(|error| panic!("sequential skip reference failed: {error:?}"));

    let state = ParallelState::new(db, true, true);
    let scheduler =
        Scheduler::new_with_runtime_config(cfg, env, txs, state, None, revm_compatibility_config());
    scheduler.parallel_execute(Some(23)).expect("parallel execute failed");
    let (actual_outcomes, mut state) = scheduler.take_result_and_state();
    let actual_bundle = state.parallel_take_bundle(BundleRetention::Reverts);

    assert_eq!(expected_outcomes, actual_outcomes, "transaction outcomes differ");
    compare_bundle_state(&expected_bundle, &actual_bundle);
    actual_outcomes
}

/// Execution-only timings from a successful parallel-vs-sequential comparison.
#[derive(Clone, Copy, Debug)]
pub struct ReplayTimings {
    /// Strictly ordered revm execution time.
    pub sequential: Duration,
    /// Grevm parallel execution time.
    pub parallel: Duration,
}

/// Same as [`compare_evm_execute_with_spec`] but takes a fully-specified [`CfgEnv`] and
/// [`BlockEnv`] (e.g. a real mainnet block).
///
/// The sequential revm reference runs **first**. Every transaction in a real mainnet block is
/// consensus-valid, so any sequential error is an invalid fixture/environment and panics. Once the
/// reference succeeds, any Grevm error, skipped transaction, or state/result divergence also
/// panics. Grevm-specific delegated-account policies are explicitly disabled via
/// [`revm_compatibility_config`].
pub fn compare_evm_execute_with_env<DB>(
    db: DB,
    txs: Vec<TxEnv>,
    cfg: CfgEnv,
    env: BlockEnv,
    parallel_metrics: HashMap<&str, usize>,
) -> ReplayTimings
where
    DB: DatabaseRef + Send + Sync + Debug,
    DB::Error: Send + Sync + Clone + Debug + 'static,
{
    let db = Arc::new(db);
    let txs = Arc::new(txs);

    // 1) Sequential reference FIRST. A mainnet transaction cannot be invalid; an error means the
    //    fixture or execution environment is wrong and must fail the replay.
    let start = Instant::now();
    let reth_result = execute_revm_sequential(db.clone(), cfg.clone(), env.clone(), &txs)
        .unwrap_or_else(|error| panic!("sequential reference execution failed: {error:?}"));
    let sequential = start.elapsed();

    // 2) Grevm parallel. The reference already succeeded, so an error here is a real grevm bug.
    // The global recorder is only for optional diagnostics. Assertions use the scheduler's own
    // collector below, so concurrent tests cannot drain or contaminate one another's metrics.
    let snapshotter =
        std::env::var_os("GREVM_PRINT_METRICS").is_some().then(metrics_snapshotter).flatten();
    let start = Instant::now();
    let state = ParallelState::new(db.clone(), true, true);
    let executor = Scheduler::new_with_runtime_config(
        cfg.clone(),
        env.clone(),
        txs.clone(),
        state,
        None,
        revm_compatibility_config(),
    );
    // set determined partitions
    executor.parallel_execute(Some(23)).expect("parallel execute failed");
    let parallel = start.elapsed();
    let observed_metrics = executor.metrics_snapshot();
    let (results, mut state) = executor.take_result_and_state();
    let parallel_result = (results, state.parallel_take_bundle(BundleRetention::Reverts));

    if let Some(snapshotter) = snapshotter {
        for (key, _, _, value) in snapshotter.snapshot().into_vec() {
            let value = match value {
                DebugValue::Counter(v) => v as usize,
                DebugValue::Gauge(v) => v.0 as usize,
                DebugValue::Histogram(v) => v.last().cloned().map_or(0, |ov| ov.0 as usize),
            };
            let name = key.key().name();
            println!("metrics: {name} => value: {value:?}");
        }
    }
    assert_expected_metrics(&observed_metrics, parallel_metrics);

    compare_execution_result(&reth_result.0, &parallel_result.0);
    compare_bundle_state(&reth_result.1, &parallel_result.1);
    ReplayTimings { sequential, parallel }
}

/// Simulate the sequential execution of transactions in reth
pub fn execute_revm_sequential<DB>(
    db: DB,
    cfg: CfgEnv,
    env: BlockEnv,
    txs: &[TxEnv],
) -> Result<(Vec<ExecutionResult>, BundleState), EVMError<DB::Error>>
where
    DB: DatabaseRef + Debug,
    DB::Error: Send + Sync + Debug + 'static,
{
    let spec = cfg.spec;
    let db = StateBuilder::new().with_bundle_update().with_database_ref(db).build();
    let evm = Context::mainnet()
        .with_db(db)
        .with_cfg(cfg)
        .with_block(env)
        .build_mainnet_with_inspector(NoOpInspector {})
        .with_precompiles(PrecompilesMap::from_static(EthPrecompiles::new(spec).precompiles));
    let mut evm = EthEvm::new(evm, false);

    let mut results = Vec::with_capacity(txs.len());
    for tx in txs {
        let result_and_state = evm.transact_raw(tx.clone()).map_err(|e| match e {
            EVMError::Transaction(t) => EVMError::Transaction(t),
            EVMError::Header(h) => EVMError::Header(h),
            EVMError::Database(inner) => EVMError::Database(inner.into_external_error()),
            EVMError::Custom(s) => EVMError::Custom(s),
            EVMError::CustomAny(a) => EVMError::CustomAny(a),
        })?;
        evm.db_mut().commit(result_and_state.state);
        results.push(result_and_state.result);
    }
    evm.db_mut().merge_transitions(BundleRetention::Reverts);

    Ok((results, evm.db_mut().take_bundle()))
}

/// Execute every transaction strictly in block order, committing successful EVM executions and
/// treating every transaction-validation error as a state-free skip.
///
/// Database errors, custom errors, and header errors remain fatal so this reference cannot
/// accidentally hide a consensus or database failure.
pub fn execute_revm_sequential_skipping_invalid<DB>(
    db: DB,
    cfg: CfgEnv,
    env: BlockEnv,
    txs: &[TxEnv],
) -> Result<(Vec<TxExecutionOutcome>, BundleState), EVMError<DB::Error>>
where
    DB: DatabaseRef + Debug,
    DB::Error: Send + Sync + Debug + 'static,
{
    let spec = cfg.spec;
    let disable_nonce_check = cfg.disable_nonce_check;
    let db = StateBuilder::new().with_bundle_update().with_database_ref(db).build();
    let evm = Context::mainnet()
        .with_db(db)
        .with_cfg(cfg)
        .with_block(env)
        .build_mainnet_with_inspector(NoOpInspector {})
        .with_precompiles(PrecompilesMap::from_static(EthPrecompiles::new(spec).precompiles));
    let mut evm = EthEvm::new(evm, false);

    let mut outcomes = Vec::with_capacity(txs.len());
    for tx in txs {
        if !disable_nonce_check && tx.nonce == u64::MAX {
            let state_nonce = match evm.db_mut().basic_ref(tx.caller) {
                Ok(info) => info.map_or(0, |info| info.nonce),
                Err(error) => {
                    return Err(EVMError::Database(error.into_external_error()));
                }
            };
            if state_nonce == u64::MAX {
                outcomes.push(TxExecutionOutcome::Skipped(
                    crate::InvalidTransaction::NonceOverflowInTransaction,
                ));
                continue;
            }
        }
        match evm.transact_raw(tx.clone()) {
            Ok(result_and_state) => {
                evm.db_mut().commit(result_and_state.state);
                outcomes.push(TxExecutionOutcome::Executed(result_and_state.result));
            }
            Err(EVMError::Transaction(error)) => {
                outcomes.push(TxExecutionOutcome::Skipped(error));
            }
            Err(EVMError::Header(error)) => return Err(EVMError::Header(error)),
            Err(EVMError::Database(error)) => {
                return Err(EVMError::Database(error.into_external_error()));
            }
            Err(EVMError::Custom(error)) => return Err(EVMError::Custom(error)),
            Err(EVMError::CustomAny(error)) => return Err(EVMError::CustomAny(error)),
        }
    }
    evm.db_mut().merge_transitions(BundleRetention::Reverts);

    Ok((outcomes, evm.db_mut().take_bundle()))
}

/// Sequentially execute `txs` against `db` under `cfg`/`env`, returning the indices of the
/// transactions that *can* execute, in order, stopping once cumulative gas reaches `gas_cap`
/// (pass `u64::MAX` for no cap).
///
/// A transaction that executes (whether it succeeds or reverts) is committed and kept; one that
/// errors — i.e. is invalid under this environment (wrong nonce, insufficient funds, fee below
/// basefee, …) — is skipped. This is used to turn a naive concatenation of many blocks'
/// transactions into a single "big block" grevm can run end-to-end: merging blocks under one
/// block environment inevitably invalidates some transactions, which must be filtered out (grevm
/// aborts the whole block on the first transaction error).
pub fn filter_successful_txs<DB>(
    db: DB,
    cfg: CfgEnv,
    env: BlockEnv,
    txs: &[TxEnv],
    gas_cap: u64,
) -> Vec<usize>
where
    DB: DatabaseRef + Debug,
    DB::Error: Send + Sync + Debug + 'static,
{
    let spec = cfg.spec;
    let db = StateBuilder::new().with_bundle_update().with_database_ref(db).build();
    let evm = Context::mainnet()
        .with_db(db)
        .with_cfg(cfg)
        .with_block(env)
        .build_mainnet_with_inspector(NoOpInspector {})
        .with_precompiles(PrecompilesMap::from_static(EthPrecompiles::new(spec).precompiles));
    let mut evm = EthEvm::new(evm, false);

    let mut kept = Vec::new();
    let mut total_gas: u64 = 0;
    for (i, tx) in txs.iter().enumerate() {
        match evm.transact_raw(tx.clone()) {
            Ok(result_and_state) => {
                total_gas = total_gas.saturating_add(result_and_state.result.tx_gas_used());
                evm.db_mut().commit(result_and_state.state);
                kept.push(i);
                if total_gas >= gas_cap {
                    break;
                }
            }
            // Invalid under the merged environment — drop it.
            Err(_) => continue,
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm_database::EmptyDB;
    use revm_primitives::TxKind;

    #[test]
    fn revm_compatibility_always_disables_delegated_safety() {
        assert_eq!(revm_compatibility_config().delegated_safety, DelegatedSafetyConfig::disabled());
    }

    #[test]
    #[should_panic(expected = "sequential reference execution failed")]
    fn strict_replay_rejects_invalid_reference_transaction() {
        let caller = Address::from([0x11; 20]);
        let tx = TxEnv {
            caller,
            kind: TxKind::Call(caller),
            gas_limit: 21_000,
            gas_price: 1,
            ..TxEnv::default()
        };
        let cfg = CfgEnv::new_with_spec(SpecId::SHANGHAI);
        let env = BlockEnv { gas_limit: 30_000_000, ..BlockEnv::default() };

        compare_evm_execute_with_env(EmptyDB::new(), vec![tx], cfg, env, Default::default());
    }

    #[test]
    #[should_panic(expected = "expected metric `grevm.missing` was not recorded")]
    fn expected_metrics_reject_missing_keys() {
        assert_expected_metrics(&BTreeMap::new(), [("grevm.missing", 1)].into_iter().collect());
    }

    #[test]
    #[should_panic(expected = "metric `grevm.total_tx_cnt` mismatch")]
    fn expected_metrics_reject_wrong_values() {
        assert_expected_metrics(
            &[("grevm.total_tx_cnt", 7)].into_iter().collect(),
            [("grevm.total_tx_cnt", 8)].into_iter().collect(),
        );
    }
}
