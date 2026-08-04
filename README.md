# Grevm

Grevm is a Block-STM-inspired optimistic parallel EVM execution engine built on
[revm](https://github.com/bluealloy/revm). It combines multi-version state, read-set validation,
and dependency-aware scheduling while preserving block-order outcomes and state.

See [Use Grevm with reth](docs/use-with-reth.md) for the public API and integration model.

## Current execution pipeline

1. Speculative workers execute transactions against multi-version memory and record their read and
   write sets.
2. Validation checks every read against the latest preceding writer and incarnation. Conflicts are
   marked as estimates and rescheduled; discovered dependencies are scheduling hints, not the
   source of correctness.
3. One finality loop publishes only the contiguous prefix whose validations remain newer than every
   relevant validation rewind.
4. One ordered-commit loop validates each original transaction nonce against committed state when
   nonce checking is enabled, folds any deferred beneficiary reward into the EVM state, commits it
   once, records the outcome, and only then publishes the new committed-prefix boundary.
5. A nonce mismatch or recoverable scheduler abort replays only the uncommitted suffix
   sequentially. Invalid transactions become ordered `Skipped` outcomes; fatal errors retain the
   successfully committed prefix.

`GrevmConfig::concurrency_level` controls the number of speculative workers. The finality and
ordered-commit loops are additional coordinator threads.

## Historical Grevm 2.1 highlights

The following results and diagram describe the Grevm 2.1 release. Static hints, Task Groups, and
the original lock-free DAG are historical designs and do not describe the current scheduler.

![Historical Grevm 2.1 design](docs/v2/images/g2design.png)

- **Grevm 2.1 achieves near-optimal performance in low-contention scenarios**, matching Block-STM with **11.25
  gigagas/s** for Uniswap workloads and outperforming it with **95% less CPU usage** in inherently non-parallelizable
  cases by **20–30%**, achieving performance close to sequential execution.
- **Breaks Grevm 1.0’s limitations in handling highly dependent transactions**, delivering a **5.5× throughput
  increase** to **2.96 gigagas/s** in **30%-hot-ratio hybrid workloads** by minimizing re-executions through **DAG-based
  scheduling** and **Task Groups**.
- **Introduces Parallel State Store**, leveraging **asynchronous execution result bundling** to **overlap and amortize
  30-60ms of post-execution overhead within parallel execution**, effectively hiding these costs within execution time.
  It also seamlessly handles **miner rewards and the self-destruct opcode** without the performance penalties of
  sequential fallbacks.
- **In-depth analysis of optimistic parallel execution** reveals the **underestimated efficiency of Block-STM** and the
  strength of **optimistic parallelism**, providing new insights into parallel execution.
- **Lock-Free DAG** (introduced in 2.1) replaces global locking with fine-grained, node-level synchronization. This
  change reduces DAG scheduling overhead by **60%** and improves overall performance by more than **30%**. In workloads
  with fast-executing transactions—such as raw and ERC20 transfers—it delivers nearly **2×** higher throughput.

## Testing

Core library tests run without optional features:

```bash
cargo test
```

The integration suites, fixtures, and benchmarks use `test-utils`:

```bash
cargo test --features test-utils
```

This runs the library unit tests plus the integration suites (`erc20`, `native_transfers`,
`uniswap`, `eip-7702`, `delegated_safety`, and mainnet replay). See
[Testing & Benchmarking](docs/testing.md) for the full guide, including how to replay real mainnet
blocks (EIP-7702 included) and the available environment-variable knobs.

## Running the Benchmark

To reproduce the synthetic gigagas benchmark:

```bash
JEMALLOC_SYS_WITH_MALLOC_CONF="thp:always,metadata_thp:always" \
NUM_EOA=<num_accounts> HOT_RATIO=<hot_ratio> DB_LATENCY_US=<latency_in_us> \
cargo bench --features test-utils --bench gigagas
```

Replace `<num_accounts>`, `<hot_ratio>`, and `<latency_in_us>` with your desired parameters. There
is also a `continuous` benchmark that runs merged real-mainnet "big blocks"; see
[Testing & Benchmarking](docs/testing.md).

## Further Details

For historical design context and benchmark analysis, refer to the versioned technical reports.
Some implementation details in those reports, such as static dependency hints, describe their
respective releases rather than the current code.

- [Grevm 2.1 Historical Tech Report](docs/v2/grevm2.1.md)
- [Grevm 1.0 Tech Report](docs/v1/README.md)
