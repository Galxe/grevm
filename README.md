# Grevm

> Grevm (both 1.0 and 2.0/2.1) is reth-ready, please see [use-with-reth.md](docs/use-with-reth.md) for more details.

Grevm is a Block-STM inspired optimistic parallel EVM execution engine that leverages DAG-based task scheduling, dynamic
dependency management, and parallel state storage to significantly boost throughput of
[revm](https://github.com/bluealloy/revm), while reducing CPU overhead in high-conflict scenarios.

![Design Diagram](docs/v2/images/g2design.png)

## **TL;DR – Highlights of Grevm 2.1**

- **Grevm 2.1 achieves near-optimal performance in low-contention scenarios**, matching Block-STM with **16.57
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
- **Lock-free DAG** replaces global locks with fine-grained node-level synchronization. Improved scheduling performance
  by 60% and overall performance by over 30% in low conflict situations.

## Architecture Overview

Grevm 2.1 is composed of three main modules:

- **Dependency Manager (DAG Manager):**  
  Constructs a directed acyclic graph (DAG) of transaction dependencies based on speculative read/write hints.

- **Execution Scheduler:**  
  Selects transactions with no dependencies (out-degree of 0) for parallel execution, groups adjacent dependent
  transactions into **task groups**, and dynamically updates dependencies to minimize re-execution.

- **Parallel State Storage:**  
  Provides an asynchronous commit mechanism with multi-version memory to reduce latency and manage miner rewards and
  self-destruct opcodes efficiently.

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

- [Grevm 2.1 Tech Report](docs/v2/grevm2.1.md)
- [Grevm 2.0 Tech Report](docs/v2/grevm2.md)
- [Grevm 1.0 Tech Report](docs/v1/README.md)
