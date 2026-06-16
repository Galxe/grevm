# Use Grevm with reth

## Add the dependency

```toml
[dependencies]
grevm = { git = "https://github.com/Galxe/grevm.git", branch = "main" }
```

## Standalone usage

Grevm's public surface is small: build a [`ParallelState`] over any read-only database, hand it to a
[`Scheduler`] together with the config/block environment and the transactions, then call
`parallel_execute`. The database only has to implement revm's read-only `DatabaseRef` trait.

```rust
use std::sync::Arc;

use grevm::{ParallelState, ParallelTakeBundle, Scheduler};
use revm::DatabaseRef;
use revm_context::{BlockEnv, CfgEnv, TxEnv};
use revm_database::states::bundle_state::BundleRetention;

fn execute_block<DB>(cfg: CfgEnv, env: BlockEnv, txs: Vec<TxEnv>, db: DB)
where
    DB: DatabaseRef + Send + Sync + 'static,
    DB::Error: Clone + Send + Sync + 'static,
{
    let db = Arc::new(db);
    let txs = Arc::new(txs);

    // with_bundle_update = true  -> track transitions so we can extract a BundleState afterwards
    // update_db_metrics  = false -> set true to record the `grevm.db_latency_us` metric
    let state = ParallelState::new(db.clone(), true, false);

    // with_hints = false -> detect dependencies on the fly (pass true to pre-parse static hints)
    // last arg           -> optional custom precompiles (`None` for stock Ethereum precompiles)
    let scheduler = Scheduler::new(cfg, env, txs, state, false, None);

    // None -> use `GREVM_CONCURRENT_LEVEL` / available cores; `Some(n)` pins the partition count
    scheduler.parallel_execute(None).expect("parallel execution failed");

    let (results, mut state) = scheduler.take_result_and_state();
    let bundle = state.parallel_take_bundle(BundleRetention::Reverts);

    // `results`: one `ExecutionResult` per transaction, in order.
    // `bundle`:  the `BundleState` to persist to your database.
    let _ = (results, bundle);
}
```

Key signatures:

```rust
impl<DB> Scheduler<DB>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    pub fn new(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        with_hints: bool,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
    ) -> Self;

    pub fn parallel_execute(&self, concurrency_level: Option<usize>)
        -> Result<(), GrevmError<DB::Error>>;

    pub fn take_result_and_state(self) -> (Vec<ExecutionResult>, ParallelState<DB>);
}

impl<DB> ParallelState<DB> {
    pub fn new(database: DB, with_bundle_update: bool, update_db_metrics: bool) -> Self;
}
```

Public items re-exported from the crate root: `Scheduler`, `ParallelState`, `ParallelCacheState`,
`ParallelBundleState`, `ParallelTakeBundle`, `GrevmError`, `fork_join_util`.

Execution is tuned by a few environment variables (`GREVM_MIN_PARALLEL_TXS`,
`GREVM_FALLBACK_SEQUENTIAL`, `GREVM_CONCURRENT_LEVEL`, `ASYNC_COMMIT_STATE`). See
[Testing & Benchmarking](testing.md#environment-variable-knobs) for the full list and a working
end-to-end harness (`src/test_utils/common/execute.rs`).

## Integration with reth

Grevm is integrated into Gravity's reth fork,
[gravity-reth](https://github.com/Galxe/gravity-reth): its EVM layer exposes a parallel executor
(`reth_evm::grevm::ParallelExecutor`) that drives block execution through the `Scheduler` shown
above, and the pipe execution layer (`reth-pipe-exec-layer-ext-v2`) uses it to execute blocks. Refer
to gravity-reth for the full node wiring; this crate provides the parallel execution engine itself.

## Metrics

Grevm reports execution metrics via the [`metrics`](https://crates.io/crates/metrics) crate (scope
`grevm`). Integrate the [Prometheus exporter](https://crates.io/crates/metrics-exporter-prometheus)
to scrape them. All entries below are histograms.

| Metric | Description |
| --- | --- |
| `grevm.total_tx_cnt` | Total number of transactions. |
| `grevm.execution_cnt` | Number of execution incarnations. |
| `grevm.validation_cnt` | Number of validation incarnations. |
| `grevm.conflict_cnt` | Number of conflict incarnations. |
| `grevm.reset_validation_idx_cnt` | Number of validation resets. |
| `grevm.useless_dependent_update` | Number of useless dependency updates. |
| `grevm.conflict_by_miner` | Conflicts caused by miner reward / self-destruct. |
| `grevm.conflict_by_error` | Conflicts caused by an EVM error. |
| `grevm.conflict_by_estimate` | Conflicts caused by an estimate (speculative read). |
| `grevm.conflict_by_version` | Conflicts caused by a version mismatch. |
| `grevm.no_dependency_txs` | Transactions executed with no dependency. |
| `grevm.one_attempt_with_dependency` | Dependent transactions finalized on the first incarnation. |
| `grevm.more_attempts_with_dependency` | Dependent transactions needing more than two incarnations. |
| `grevm.conflict_txs` | Number of conflicting transactions. |
| `grevm.execution_time` | Execution time (nanoseconds). |
| `grevm.commit_time` | Commit time (nanoseconds). |
| `grevm.total_time` | Total time (nanoseconds). |
| `grevm.db_latency_us` | Database read latency (microseconds); recorded only when `ParallelState` is created with `update_db_metrics = true`. |
