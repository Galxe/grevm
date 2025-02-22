# Use Grevm with reth

## Import

Add the following line to your Cargo.toml

```rust
[dependencies]
grevm = { git = "https://github.com/Galxe/grevm.git", branch = "main" }
```

## Usage

Using Grevm is straightforward and efficient. You just need to create a `GrevmScheduler` and then call `GrevmScheduler::parallel_execute` to get the results of parallel transaction execution. That’s all there is to it—simple and streamlined, making the process remarkably easy.

```rust
grevm::scheduler
impl<DB> GrevmScheduler<DB>
where
  DB: DatabaseRef + Send + Sync + 'static,
  DB::Error: Send + Sync,
pub fn new(spec_id: SpecId, env: Env, db: DB, txs: Vec<TxEnv>) -> Self

pub fn parallel_execute(mut self) -> Result<ExecuteOutput, GrevmError<DB::Error>
```

In the code above, the generic constraint for `DB` is relatively relaxed; `DB` only needs to implement `DatabaseRef`, a read-only database trait provided by `revm` for Ethereum Virtual Machine interactions. Because this is a read-only interface, it naturally satisfies the `Send` and `Sync` traits. However, the `'static` constraint, which prevents `DB` from containing reference types (`&`), may not be met by all `DB` implementations. This `'static` requirement is introduced by `tokio` in Grevm, as `tokio` requires that objects have lifetimes independent of any references.

Grevm addresses this limitation by ensuring that internal object lifetimes do not exceed its own, offering an `unsafe` alternative method, `new_grevm_scheduler`, for creating `GrevmScheduler`. This method allows developers to bypass the `'static` constraint if they are confident in managing lifetimes safely.

```rust
pub fn new_grevm_scheduler<DB>(
    spec_id: SpecId,
    env: Env,
    db: DB,
    txs: Arc<Vec<TxEnv>>,
    state: Option<Box<State>>,
) -> GrevmScheduler<DatabaseWrapper<DB::Error>>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
```

### Introduce Grevm to Reth

Reth provides the `BlockExecutorProvider` trait for creating `Executor` for executing a single block in live sync and `BatchExecutor` for executing a batch of blocks in historical sync. However, the associated type constraints in `BlockExecutorProvider` do not meet the `DB` constraints required for parallel block execution. To avoid imposing constraints on the `DB` satisfying `ParallelDatabase` on all structs that implement the `BlockExecutorProvider` trait in Reth, we have designed the `ParallelExecutorProvider` trait for creating `Executor` and `BatchExecutor` that satisfy the `ParallelDatabase` constraint. We have also extended the `BlockExecutorProvider` trait with a `try_into_parallel_provider` method. If a `BlockExecutorProvider` needs to support parallel execution, the implementation of `try_into_parallel_provider` is required to return a struct that implements the `ParallelExecutorProvider` trait; otherwise, it defaults to return `None`. Through this design, without imposing stronger constraints on the `DB` in the `BlockExecutorProvider` trait, developers are provided with the optional ability to extend `BlockExecutorProvider` to produce parallel block executors. For `EthExecutorProvider`, which provides executors to execute regular ethereum blocks, we have implemented `GrevmExecutorProvider` to extend its capability for parallel execution of ethereum blocks. `GrevmExecutorProvider` can create `GrevmBlockExecutor` and `GrevmBatchExecutor` that execute blocks using Grevm parallel executor.

```rust
// crates/evm/src/execute.rs

pub trait BlockExecutorProvider: Send + Sync + Clone + Unpin + 'static {
    ...
    type ParallelProvider<'a>: ParallelExecutorProvider;

    /// Try to create a parallel provider from this provider.
    /// Return None if the block provider does not support parallel execution.
    fn try_into_parallel_provider(&self) -> Option<Self::ParallelProvider<'_>> {
        None
    }
}

/// A type that can create a new parallel executor for block execution.
pub trait ParallelExecutorProvider {
    type Executor<DB: ParallelDatabase>: for<'a> Executor<
        DB,
        Input<'a> = BlockExecutionInput<'a, BlockWithSenders>,
        Output = BlockExecutionOutput<Receipt>,
        Error = BlockExecutionError,
    >;

    type BatchExecutor<DB: ParallelDatabase>: for<'a> BatchExecutor<
        DB,
        Input<'a> = BlockExecutionInput<'a, BlockWithSenders>,
        Output = ExecutionOutcome,
        Error = BlockExecutionError,
    >;

    fn executor<DB>(&self, db: DB) -> Self::Executor<DB>
    where
        DB: ParallelDatabase;

    fn batch_executor<DB>(&self, db: DB) -> Self::BatchExecutor<DB>
    where
        DB: ParallelDatabase;
}
```

When there is a need to execute a block, we expect developers to call `BlockExecutorProvider::try_into_parallel_provider` and decide whether to use the parallel execution solution provided by `BlockExecutorProvider` based on the returned value, or resort to sequential block execution if there is no corresponding parallel execution implementation in `BlockExecutorProvider`. By making such calls, we can also control the return value of `try_into_parallel_provider` by setting the environment variable `EVM_DISABLE_GREVM`, reverting to Reth's native executor based on revm for block execution. This feature is handy for conducting comparative experiments.

```rust
// crates/blockchain-tree/src/chain.rs
// AppendableChain::validate_and_execute
let state = if let Some(parallel_provider) =
    externals.executor_factory.try_into_parallel_provider()
{
    parallel_provider.executor(db).execute((&block, U256::MAX).into())?
} else {
    externals.executor_factory.executor(db).execute((&block, U256::MAX).into())?
};
```

```rust
// crates/blockchain-tree/src/chain.rs
// ExecutionStage::execute
let mut executor =
    if let Some(parallel_provider) = self.executor_provider.try_into_parallel_provider() {
        EitherBatchExecutor::Parallel(parallel_provider.batch_executor(Arc::new(db)))
    } else {
        EitherBatchExecutor::Sequential(self.executor_provider.batch_executor(db))
    };
```

The integration code for connecting Grevm to Reth is already complete. You can check it out here: [Introduce Grevm to Reth](https://github.com/Galxe/grevm-reth/commit/9473670ab3c02e3699af1413f9c7bffc4bbbee45)

## Metrics

To monitor and observe execution performance, Grevm uses the `metrics` crate from [crates.io](https://crates.io/crates/metrics). Users can integrate the [Prometheus exporter](https://crates.io/crates/metrics-exporter-prometheus) within their code to expose metrics data, which can then be configured in Grafana for real-time monitoring and visualization. Below is a table that provides the names and descriptions of various metric indicators:

| Metric Name | Description |
| --- | --- |
| grevm.parallel_round_calls | Number of times parallel execution is called. |
| grevm.sequential_execute_calls | Number of times sequential execution is called. |
| grevm.total_tx_cnt | Total number of transactions. |
| grevm.parallel_tx_cnt | Number of transactions executed in parallel. |
| grevm.sequential_tx_cnt | Number of transactions executed sequentially. |
| grevm.finality_tx_cnt | Number of transactions that encountered conflicts. |
| grevm.conflict_tx_cnt | Number of transactions that reached finality. |
| grevm.unconfirmed_tx_cnt | Number of transactions that are unconfirmed. |
| grevm.reusable_tx_cnt | Number of reusable transactions. |
| grevm.skip_validation_cnt | Number of transactions that skip validation |
| grevm.concurrent_partition_num | Number of concurrent partitions. |
| grevm.partition_execution_time_diff | Execution time difference between partitions(in nanoseconds). |
| grevm.partition_num_tx_diff | Number of transactions difference between partitions. |
| grevm.parse_hints_time | Time taken to parse execution hints(in nanoseconds). |
| grevm.partition_tx_time | Time taken to partition transactions(in nanoseconds). |
| grevm.parallel_execute_time | Time taken to validate transactions(in nanoseconds). |
| grevm.validate_time | Time taken to execute(in nanoseconds). |
| grevm.merge_write_set_time | Time taken to merge write set(in nanoseconds). |
| grevm.commit_transition_time | Time taken to commit transition(in nanoseconds). |
| grevm.build_output_time | Time taken to build output(in nanoseconds). |