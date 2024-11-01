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
    txs: Vec<TxEnv>,
) -> GrevmScheduler<DatabaseWrapper<DB::Error>>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Send + Sync,
```

### Introduce Grevm to Reth

Reth provides a `BlockExecutorProvider` to create an `Executor` for executing transactions. In theory, integrating Grevm should only require creating an `EthGrevmExecutor`. However, `BlockExecutorProvider` imposes a `DB` constraint that uses the `Database` trait, which is a mutable interface and doesn’t align with Grevm’s read-only requirements. In practice, the passed `DB` parameter does fulfill the `DatabaseRef` trait, which Grevm requires. Therefore, it’s necessary to add a `parallel_executor` to `BlockExecutorProvider` to introduce a `DatabaseRef` constraint.

```rust
pub trait ParallelDatabase: DatabaseRef<Error: Send + Sync> + Send + Sync {}

impl<EvmConfig> BlockExecutorProvider for EthExecutorProvider<EvmConfig>
where
    EvmConfig: ConfigureEvm,
{
    fn parallel_executor<DB>(&self, db: DB) -> Self::ParallelExecutor<DB>
    where
        DB: ParallelDatabase<Error: Into<ProviderError> + Display + Clone>,
    {
        EthGrevmExecutor::new(self.chain_spec.clone(), self.evm_config.clone(), db)
    }
}
```

The integration code for connecting Grevm to Reth is already complete. You can check it out here: [Introduce Grevm to Reth](https://github.com/Galxe/grevm-reth/commit/52472e6c6b125d5f038e93f6c5eddc57b230ba66)

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