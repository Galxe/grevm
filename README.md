# Grevm

> Grevm (both 1.0 and 2.0) is reth-ready, please see [use-with-reth.md](use-with-reth.md) for more details.

Grevm is a Block-STM inspired optimistic parallel EVM execution engine that leverages DAG-based task scheduling, dynamic
dependency management, and parallel state storage to significantly boost throughput of
[revm](https://github.com/bluealloy/revm), while reducing CPU overhead in high-conflict scenarios.

![Design Diagram](docs/v2/images/g2design.png)

## Architecture Overview

Grevm 2.0 is composed of three main modules:

- **Dependency Manager (DAG Manager):**  
  Constructs a directed acyclic graph (DAG) of transaction dependencies based on speculative read/write hints.

- **Execution Scheduler:**  
  Selects transactions with no dependencies (out-degree of 0) for parallel execution, groups adjacent dependent
  transactions into **task groups**, and dynamically updates dependencies to minimize re-execution.

- **Parallel State Storage:**  
  Provides an asynchronous commit mechanism with multi-version memory to reduce latency and manage miner rewards and
  self-destruct opcodes efficiently.

## Benchmark Highlights

- **Conflict-Free Workloads:**  
  Achieves significant speedups over sequential execution (up to ~50× in some cases) especially when I/O latencies are
  introduced.

- **Contention (Hybrid) Workloads:**  
  With a 30% hot ratio, Grevm 2.0 outperforms Grevm 1.0 by 5.55× and reaches a throughput of **2.96 gigagas/s**.

## Running the Benchmark

To reproduce the benchmark:

```bash
JEMALLOC_SYS_WITH_MALLOC_CONF="thp:always,metadata_thp:always" \
NUM_EOA=<num_accounts> HOT_RATIO=<hot_ratio> DB_LATENCY_US=<latency_in_us> \
cargo bench --bench gigagas
```

Replace `<num_accounts>`, `<hot_ratio>`, and `<latency_in_us>` with your desired parameters.

## Further Details

For a comprehensive explanation of the design, algorithmic choices, and in-depth benchmark analysis, please refer to the
full technical report.

- [Grevm 2.0 Tech Report](docs/v2/grevm2.md)
- [Grevm 1.0 Tech Report](docs/v1/README.md)
