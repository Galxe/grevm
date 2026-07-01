use alloy_evm::{EthEvm, Evm, precompiles::PrecompilesMap};
use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};

use crate::{ParallelState, ParallelTakeBundle, Scheduler};
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
/// Returns `None` if metric capture isn't requested or another global recorder is already
/// installed.
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

pub fn compare_execution_result(left: &Vec<ExecutionResult>, right: &Vec<ExecutionResult>) {
    for (i, (left_res, right_res)) in left.iter().zip(right.iter()).enumerate() {
        assert_eq!(left_res, right_res, "Tx {}", i);
    }
    assert_eq!(left.len(), right.len());
}

pub fn compare_evm_execute<DB>(
    db: DB,
    txs: Vec<TxEnv>,
    with_hints: bool,
    disable_nonce_check: bool,
    parallel_metrics: HashMap<&str, usize>,
) where
    DB: DatabaseRef + Send + Sync + Debug,
    DB::Error: Send + Sync + Clone + Debug + 'static,
{
    compare_evm_execute_with_spec(
        db,
        txs,
        with_hints,
        disable_nonce_check,
        parallel_metrics,
        SpecId::SHANGHAI,
    );
}

/// Same as [`compare_evm_execute`] but lets the caller pick the [`SpecId`]. Needed for
/// fork-gated features such as EIP-7702 (active from [`SpecId::PRAGUE`]).
pub fn compare_evm_execute_with_spec<DB>(
    db: DB,
    txs: Vec<TxEnv>,
    with_hints: bool,
    disable_nonce_check: bool,
    parallel_metrics: HashMap<&str, usize>,
    spec: SpecId,
) where
    DB: DatabaseRef + Send + Sync + Debug,
    DB::Error: Send + Sync + Clone + Debug + 'static,
{
    let mut env = BlockEnv::default();
    env.beneficiary = super::account::MINER_ADDRESS;
    let mut cfg = CfgEnv::new_with_spec(spec);
    cfg.disable_nonce_check = disable_nonce_check;
    // Synthetic blocks are constructed to always execute, so a sequential failure is itself a bug.
    match compare_evm_execute_with_env(db, txs, with_hints, cfg, env, parallel_metrics) {
        ReplayOutcome::Ok { .. } => {}
        ReplayOutcome::SequentialFailed(e) => panic!("sequential reference execution failed: {e}"),
    }
}

/// Outcome of a parallel-vs-sequential comparison.
#[derive(Debug)]
pub enum ReplayOutcome {
    /// Grevm parallel execution matched the sequential revm reference (per-tx results + bundle
    /// state asserted equal). Carries the **execution-only** wall-clock time of each path (the
    /// inputs are already in memory, so neither figure includes any file/RPC I/O) — useful for
    /// comparing parallel vs sequential throughput.
    Ok { sequential: Duration, parallel: Duration },
    /// The **sequential** reference itself errored, so the inputs can't be executed at all (e.g. an
    /// incomplete fixture). This is *not* a grevm divergence — callers replaying real blocks should
    /// skip the block rather than blame the parallel scheduler. Carries the error description.
    SequentialFailed(String),
}

/// Same as [`compare_evm_execute_with_spec`] but takes a fully-specified [`CfgEnv`] and
/// [`BlockEnv`] (e.g. a real mainnet block).
///
/// The sequential revm reference runs **first**: if it can't execute the block the inputs are
/// unreplayable and [`ReplayOutcome::SequentialFailed`] is returned without touching grevm. Only
/// when the reference succeeds does grevm's parallel scheduler run; any error or divergence there
/// is a genuine grevm bug and panics.
pub fn compare_evm_execute_with_env<DB>(
    db: DB,
    txs: Vec<TxEnv>,
    with_hints: bool,
    cfg: CfgEnv,
    env: BlockEnv,
    parallel_metrics: HashMap<&str, usize>,
) -> ReplayOutcome
where
    DB: DatabaseRef + Send + Sync + Debug,
    DB::Error: Send + Sync + Clone + Debug + 'static,
{
    let db = Arc::new(db);
    let txs = Arc::new(txs);

    // 1) Sequential reference FIRST — if it errors, the inputs are unreplayable (not a grevm
    //    issue). Timed: the DB is already in memory, so this is pure execution, no I/O.
    let start = Instant::now();
    let reth_result = match execute_revm_sequential(db.clone(), cfg.clone(), env.clone(), &*txs) {
        Ok(result) => result,
        Err(e) => return ReplayOutcome::SequentialFailed(format!("{e:?}")),
    };
    let sequential = start.elapsed();

    // 2) Grevm parallel. The reference already succeeded, so an error here is a real grevm bug.
    // Capture metrics when requested (`GREVM_PRINT_METRICS`, set by `replay_mainnet`) or when the
    // caller asserts on them. The recorder is installed before execution so all of grevm's metrics
    // land in it, then snapshotted right after so we read *this* block's values.
    let want_metrics =
        std::env::var_os("GREVM_PRINT_METRICS").is_some() || !parallel_metrics.is_empty();
    let snapshotter = want_metrics.then(metrics_snapshotter).flatten();

    let start = Instant::now();
    let state = ParallelState::new(db.clone(), true, true);
    let executor = Scheduler::new(cfg.clone(), env.clone(), txs.clone(), state, with_hints, None);
    // set determined partitions
    executor.parallel_execute(Some(23)).expect("parallel execute failed");
    let parallel = start.elapsed();
    let (results, mut state) = executor.take_result_and_state();
    let parallel_result = (results, state.parallel_take_bundle(BundleRetention::Reverts));

    if let Some(snapshotter) = snapshotter {
        for (key, _, _, value) in snapshotter.snapshot().into_vec() {
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
    }

    compare_execution_result(&reth_result.0, &parallel_result.0);
    compare_bundle_state(&reth_result.1, &parallel_result.1);
    ReplayOutcome::Ok { sequential, parallel }
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
        // v40 wraps DB errors in `EvmDatabaseError`; the outer signature keeps the historical
        // `EVMError<DB::Error>` shape so unwrap the wrapper here.
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
