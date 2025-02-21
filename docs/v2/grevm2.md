# Grevm 2.0

**TLD;R**

**TODO**

- Comparing with 1.0
  - Grevm 2.0 resolves known performance limit with Highly dependent transactions, achieving 5.5x throughput increase to
    2.96 gigagas/s for Hybrid workload, 30% hot ratio.
- Comparing with Block-STM
  - Grevm 2.0 achieves identical performance for low-conflict workloads, and outperforms Block-STM in extreme cases by
    having identical performance of sequential execution, instead of 20-30% slower, with TODO% less CPU utilization.

## Abstract

Grevm 2.0 marries a task scheduling mechanism based on a **directed acyclic graph (DAG)** of transaction dependencies,
based on simulated transaction execution results. Compared to Block-STM's approach of scheduling tasks purely based on
the lowest index, this design offers two advantages: first, it can significantly reduces possibility of transaction
re-execution caused by dependency conflicts, by clustering adjacently dependent transitions into **task groups**;
second, it allows high-index transactions without dependencies to be executed in parallel earlier, improving concurrency
efficiency. These two advantages make Grevm 2.0 more efficient in high-conflict scenarios, significantly reducing the
number of re-executions and reducing total execution time and CPU utilization. Our benchmark results show that:
comparing with 1.0, for a hybrid workload of Uniswap, ERC20 and transfer, with a 30% hot ratio, Grevm 2.0 achieves 5.5x
higher throughput than Grevm 1.0, reaching **2.96 gigagas/s**, and comparing with Block-STM, Grevm 2.0 achieves
identical performance for low-conflict workloads, and outperforms Block-STM in extreme cases by having identical
performance of sequential execution, instead of 20-30% slower.

Engineering-wise, Grevm 2.0 introduces **parallel state**, an asynchronous commit that bundles the final execution
result in parallel, which also elegantly solves the challenges of miner rewards and self-destruct opcode without
compromising correctness and performance penalty of falling back to sequential.

In this report, we will first introduce the design of Grevm 2.0, including our insights found in the process of
experiments, including the rationale behind the design choices, and some rarely revealed data showcasing the impressive
efficiency of Block-STM's optimistic execution. Then, we will present the benchmark results.

## Algorithm Design

The core architecture of Grevm2.0 is composed of three main modules: **Dependency Manager (DAG manager)**, **Execution
Scheduler**, and **Parallel State Storage**. In a nutshell, Grevm2.0 employs a DAG-driven task scheduling mechanism to
(1) group adjacent dependent transactions into task groups, and (2) execute transactions with the smallest index that
has no dependencies (i.e., out-degree of 0), with a selective dependency update strategy.

![image.png](images/g2design.png)

### Dependency Manager And Execution Scheduler

The dependency manager in Grevm 2.0 is responsible for tracking and resolving dependencies between transactions to
enable efficient parallel execution scheduling. Similar to 1.0, Grevm 2.0 constructs a Directed Acyclic Graph (DAG)
where:

- **Nodes represent transactions**, identified by unique indices. Let T_i be a transaction with index i.
- **Edges denote dependencies**, that dependency edge T_j to T_i exists if T_j writes data that T_i reads, indicating a
  read-after-write data dependency. Note that only transactions with higher indices may depend on those with lower
  indices.

Before parallel execution, dependencies are inferred using **hints** that indicate speculated read/write sets. Hints are
obtained from static analysis or simulation (executing transactions on the last committed state).

The **Execution Scheduler** module handles both transaction execution and validation, following this workflow:

1. **Parallel Execution**: The Scheduler selects and executes the transaction with the smallest identifier from the
   Dependency DAG that has no dependencies (i.e., an out-degree of 0).
2. **Validation**: Once execution is complete, the transaction enters a pending validation phase. The Scheduler then
   checks whether the read set of the transaction with the smallest identifier has changed:
   - If the read set remains unchanged, the transaction moves to the **Unconfirmed** state. Consecutive Unconfirmed
     transactions will eventually transition into the **Finality** state, from the consecutive lowest index starting
     from 0.
   - If the read set has changed, the transaction is marked as **Conflict**, reinserted into the DAG, and its
     dependencies are updated before being scheduled for re-execution.

During scheduling, when consecutive transactions have dependencies, they can be grouped into a **Task Group**.
Transitions in a task group will be scheduled together and executed sequentially. For instance, in the example above,
**tx2** depends on **tx3**. Normally, **tx2** must finish execution before **tx3** can begin. However, by bundling them
into a Task Group, they can be executed sequentially within the same thread, ensuring that **tx3** correctly reads the
updated data from **tx2**. Task groups effectively handle high-conflict scenarios, particularly when consecutive
transactions are strongly interdependent, reducing frequent task switching, re-execution and scheduling overheads. We
believe that this is a common case for highly dependent workloads like NFT minting, DeFi transactions, and especially
when we do not have a strategy to **re-order** transactions to maximize parallelism. Especially in the worst-case
scenario where all transitions are interdependent, forming a chain, task group execution can remain as efficient as
serial execution. This design is simple and effective, by grouping tightly coupled transactions, the scheduling logic is
streamlined, avoiding introducing excessive scheduling complexity.

During execution, dependencies are updated dynamically based on actual transaction behavior.

For adding dependencies, Grevm 2.0 follows a **selective dependency strategy**, adding only the most necessary edges to
minimize DAG modifications.

There are three scenarios trigger adding dependency:

1. When a transaction reads estimated data that later proves incorrect, instead of immediately aborts the transaction
   and marks it as dependent, Grevm 2.0 will complete execution, analyze the actual read-write set, and adds **only the
   most significant dependency** (i.e., the transaction with the largest index that it depends on).
2. When a transaction T*j reads from a **miner account** or a **self-destructed account**, but the parallel state has
   not been committed for T*{j-1}. Instead of linking to all prior transactions, we only add T\_{j-1} as a dependency,
   reducing redundant edges.
3. When the read set of a transaction is updated during _validation_. Grevm 2.0 recalculates dependencies and adds only
   the most recent transaction as a dependency. That is, If T_i detects a conflict with a set of transactions, find the
   transaction with the **largest index k **. Add the dependency edge T_k to T_i.

The rationale behind this design is to minimize the performance overhead of updating the DAG while ensuring efficient
scheduling and correctness.

As for dependency removal, the timing is a key optimization factor that directly affects both scheduling efficiency and
the likelihood of transaction re-execution. Dependencies can be removed at three different stages: Execution,
Validation, or Finality, each with trade-offs. Removing dependencies after Execution speeds up the scheduling of
subsequent transactions but increases the risk of re-execution; removing them after Validation strikes a balance between
re-execution probability and scheduling delay; and removing them after Finality ensures that transactions do not need to
be re-executed but results in the longest scheduling delay.

To achieve the best trade-off, we adopt a hybrid Execution + Finality strategy: dependencies identified before parallel
execution are removed immediately after Execution to maximize the throughput of scheduler, while dynamically detected
dependencies are removed after Finality to minimize re-execution. This approach enables efficient scheduling when hints
are accurate and reduce re-execution risks when hints are less reliable. Based on our empirical study and experiments,
it balances scheduling speed and re-execution probability in many scenarios, achieving best performance out of both
worlds.

### Parallel State Storage

![image.png](images/image%201.png)

The **Storage** module implements **DatabaseRef** and offers two functionalities: **Parallel State** and **Multi-Version
Memory**. It enhances system performance by introducing a **Parallel State** layer between EvmDB and MvMemory. The
process works as follows: After transaction T_i completes execution, its results are first stored in MvMemory. Once it
reaches the Finality state, an asynchronous task is triggered to commit the results to Parallel State. When transaction
T_j accesses data, it reads from Parallel State if dealing with a miner or self-destructed account; otherwise, it
retrieves data from MvMemory. There design offers several advantages:

- **Amortized Result State Building**: Committed transaction will continuously generate reth-compatible result states,
  removing 50-150ms latency of bundling them after the whole block is executed.
- **State Management**: Because this layer implements all
  [EVM State](https://github.com/bluealloy/revm/blob/main/crates/database/src/states/state.rs) interfaces, transactions
  can retrieve the latest state of miner or self-destructed accounts without relying entirely on Multi-Version Memory.
  When a transaction executes a self-destruct operation, conflicts can occur if later transactions attempt to access the
  self-destructed account. The self-destruct transaction updates MvMemory's write set and marks the account as
  self-destructed. If a later transaction retrieves self-destructed account data from MvMemory, it must re-fetch the
  latest state from Parallel State. In both cases, Parallel State's commit ID serves as the version number, ensuring
  that for transaction **j**, only data with version **j-1** is deemed valid—otherwise, the transaction is marked as
  conflicting

## Benchmark

We evaluate the Grevm 2.0 implementation with the same setup as 1.0:

- aws c7g.8xlarge 32 vCPUs @2.6 GHz
- Ubuntu 22.04.5 LTS
- cargo 1.81.0 (2dbb1af80 2024-08-20)
- Grevm git commit hash: `ccf5cdc0488e8edf4dc9e86e11b8e3e595c8fd6d`
- pevm git commit hash: `d48fae90b6ad36ddc5d613ee28ad23214353e81e`
- Benchmark code:
  [https://github.com/Galxe/grevm/blob/main/benches/gigagas.rs](https://github.com/Galxe/grevm/blob/main/benches/gigagas.rs)

To reproduce the benchmark, run

```bash
JEMALLOC_SYS_WITH_MALLOC_CONF="thp:always,metadata_thp:always" NUM_EOA=${NUM_EOA} HOT_RATIO=${HOT_RATIO} DB_LATENCY_US=${DB_LATENCY_US} cargo bench --bench gigagas
```

Replace `${NUM_EOA}`, `${HOT_RATIO}`, and `${DB_LATENCY_US}` with the desired parameters:

- `NUM_EOA`: Number of accounts.
- `HOT_RATIO`: Ratio of transactions accessing common ("hot") account.
- `DB_LATENCY_US`: Simulated database latency in microseconds.

### Gigagas Block Test

We conducted the same gigagas block test as 1.0, a benchmark designed to evaluate the efficiency of parallel execution
under varying workloads and conditions. Each mock block contains transactions totaling **1 gigagas** in gas consumption.
The transactions include vanilla Ether transfers, ERC20 token transfers, and Uniswap swaps. Pre-state data is stored
entirely in-memory to isolate execution performance from disk I/O variability. To mimic real-world conditions where disk
I/O latency can impact performance, we introduced artificial latency using the `db_latency` parameter.

### Conflict-Free Transactions

We first evaluated our implementation using conflict-free workloads to measure optimal performance. In this scenario,
transactions consist of independent raw transfers, ERC20 transfers, and Uniswap swaps, each involving separate contracts
and accounts to ensure no data dependencies. This setup provides a baseline to assess the maximum achievable performance
improvement through parallel execution without the impact of transaction conflicts.

| Test Case       | Num Txs | DB Latency | Sequential | Grevm 1.0 | Grevm 2.0 | Speedup | Gigagas/s |
| --------------- | ------- | ---------- | ---------- | --------- | --------- | ------- | --------- |
| Raw Transfers   | 47620   | 0          | 185.74     | 69.172    | 123.01    | 1.5     | 8.13      |
|                 |         | 20us       | 3703.5     | N/A       | 125.59    | 29.5    | 7.96      |
|                 |         | 40us       | 4654.0     | N/A       | 127.13    | 36.6    | 7.87      |
|                 |         | 60us       | 5612.0     | N/A       | 131.50    | 42.7    | 7.60      |
|                 |         | 80us       | 6560.6     | N/A       | 136.92    | 47.9    | 7.30      |
|                 |         | 100us      | 7511.1     | 179.03    | 152.96    | 49.1    | 6.54      |
| ERC20 Transfers | 33628   | 0          | 329.55     | 96.559    | 105.51    | 3.1     | 9.48      |
|                 |         | 20us       | 5297.6     | N/A       | 119.59    | 44.3    | 8.36      |
|                 |         | 40us       | 6643.3     | N/A       | 140.24    | 47.4    | 7.13      |
|                 |         | 60us       | 7992.8     | N/A       | 161.59    | 49.5    | 6.19      |
|                 |         | 80us       | 9335.1     | N/A       | 182.84    | 51.1    | 5.47      |
|                 |         | 100us      | 10681      | 243.27    | 205.30    | 52.0    | 4.87      |
| Uniswap Swaps   | 6413    | 0          | 771.83     | 108.2     | 60.36     | 12.8    | 16.57     |
|                 |         | 20us       | 12188      | N/A       | 203.70    | 59.8    | 4.91      |
|                 |         | 40us       | 15261      | N/A       | 249.38    | 61.2    | 4.01      |
|                 |         | 60us       | 18378      | N/A       | 299.91    | 61.3    | 3.33      |
|                 |         | 80us       | 21433      | N/A       | 351.29    | 61.0    | 2.85      |
|                 |         | 100us      | 24530      | 439.89    | 403.18    | 60.8    | 2.48      |

_Table 1: Grevm 2.0 Conflict-Free Transaction Execution Speedup (uint = milliseconds)_

Comparing with 1.0, when the complexity of transaction is low, Grevm 2.0 does not achieve the same level of speed up as
1.0. This is an expected result, as the DAG scheduling incurs higher overhead than 1.0's partition algorithm. However,
as the complexity of transaction increases, Grevm 2.0's performance becomes similar and a bit better than 1.0. For
example, in the Uniswap swap test, with 100us access latency, Grevm 2.0 achieves a 60.8x speedup, compared to 1.0's
58.06x. Because even for test cases that 2.0 is not as efficient as 1.0, i.e. raw and ERC20 transfers, the throughput is
still significantly higher than 1 gigagas/s, we believe this is the right trade-off for production workloads.

Similar to 1.0, the introduction of I/O latency amplified the performance advantages of parallel execution. Grevm 2.0
also harnesses the benefits of asynchronous I/O, brought possible by the parallel execution, achieving a significant
speedup over sequential execution.

Note that, with parallel state storage in place, all tests now includes extra overheads that was excluded in 1.0, such
as state bundling. This is also the reason why the cited Grevm 1.0 benchmark results was the end-to-end execution time,
found in table 3 of the original paper.

### Contention Transactions

We uses the same setup for contention transactions as 1.0, with a **hot ratio** parameter to simulate contention in
transaction workloads. This parameter allows us to model skewed access patterns, where certain accounts or contracts are
accessed more frequently than others.

- **Number of User Accounts**: **100,000** accounts used in the test.
- **Hot Ratio**: Defines the probability that a transaction will access one of the hot accounts.
  - **Hot Ratio = 0%**: Simulates a uniform workload where each read/write accesses random accounts.
  - **Hot Ratio > 0%**: Simulates a skewed workload by designating 10% of the total accounts as **hot accounts**. Each
    read/write operation has a probability equal to the hot ratio of accessing a hot account.

We also introduced a a test set called **Hybrid**, consisting of

- **60% Native Transfers**: Simple Ether transfers between accounts.
- **20% ERC20 Transfers**: Token transfers within three ERC20 token contracts.
- **20% Uniswap Swaps**: Swap transactions within two independent Uniswap pairs.

| Test            | Num Txs | Total Gas     |
| --------------- | ------- | ------------- |
| Raw Transfers   | 47,620  | 1,000,020,000 |
| ERC20 Transfers | 33,628  | 1,161,842,024 |
| Hybrid          | 36,580  | 1,002,841,727 |

_Table 2: Contention Transactions Execution Test Setup_

Recall that in 1.0, the performance of contention transactions was limited by the high degree of transaction
interdependence. In Grevm 2.0, we see a significant improvement in performance, particularly in high-conflict scenarios.
When the hot ratio is 30%, Grevm 2.0 is signally faster in all test cases except for ERC20 transfers with zero latency
due to experimental variance. The speedup is most pronounced in the Hybrid test case, where Grevm 2.0 achieves a 29x
speedup over sequential, 5.55x over Grevm 1.0, reaching a throughput of **2.96 gigagas/s**.

| Test Case       | Num Txs | DB Latency | Sequential | Grevm 1.0 | Grevm 2.0 | Total Speedup | ThroughPut(Gigagas/s) |
| --------------- | ------- | ---------- | ---------- | --------- | --------- | ------------- | --------------------- |
| Raw Transfers   | 47620   | 0          | 228.03     | 171.65    | 130.17    | 1.8           | 7.68                  |
|                 |         | 100us      | 8933.0     | 4328.08   | 199.62    | 44.8          | 5.01                  |
| ERC20 Transfers | 33628   | 0          | 366.76     | 92.07     | 110.10    | 3.33          | 9.08                  |
|                 |         | 100us      | 11526      | 438.03    | 224.69    | 51.3          | 4.45                  |
| Hybrid          | 36580   | 0          | 333.66     | 220.14    | 244.93    | 1.4           | 4.08                  |
|                 |         | 100us      | 9799.9     | 1874.7    | 337.42    | 29.0          | 2.96                  |

_Table 3: Grevm 2.0 Contention Transactions Execution Speedup (unit = milliseconds, hot ratio = 30%)_

## Comparison, Analysis, and Insights

### Optimistic Parallelism (Block-STM) Is MORE Efficient Than Expected

In parallel block execution, the dependencies between transactions play a crucial role in system performance. To
quantify this impact, we introduce two key metrics: **Dependency Distance** and **Dependent Ratio**. If a transaction
**txⱼ** depends on a preceding transaction **txᵢ**, their dependency distance is defined as:

```math
\text{dependency\_distance} = j - i
```

where **j** and **i** are the transaction indices. The number of transactions within a block that have dependencies is
denoted as **with_dependent_txs**, and the dependent ratio is given by:

```math
\text{dependent\_ratio} = \frac{\text{with\_dependent\_txs}}{\text{block\_txs}}
```

where **block_txs** represents the total number of transactions in the block. The following table illustrates the
relationship between conflict rate and dependency distance under **fully optimistic execution**, based on a **1Gigagas**
block containing **47,620** normal transfer transactions:

![dependent_distance_plot.svg](images/dependent_distance_plot.svg)

Analysis of 1 Gigagas transfer transactions reveals that even when **dependency_distance = 1**, later transactions still
have a certain probability of reading the correct data from earlier ones. When **dependency_distance ≥ 4**, conflict
rates drop significantly, showing an approximately inverse relationship with dependency distance.

This insight is highly practical: even in blocks with a high number of interdependent transactions, optimistic execution
strategies (such as **Block-STM**) do not necessarily lead to excessive transaction re-execution. This is because
transactions with greater dependency distances are less likely to conflict, reducing the performance cost of optimistic
execution.

This principle provides a theoretical foundation for optimizing parallel execution engines, enabling maximum performance
while ensuring correctness. For transactions with **short dependency distances (dependency_distance ≤ 3)**, a more
conservative scheduling strategy (e.g., **Task Group**) can help reduce conflicts. Meanwhile, transactions with **larger
dependency distances** can be processed using fully optimistic execution to maximize parallelism and throughput.

In designing a parallel execution engine, dependency distance analysis can guide scheduling optimizations. For example,
transactions with shorter dependency distances may be given higher priority or stricter conflict detection, while those
with larger dependency distances can adopt a more relaxed optimistic execution approach. This strategy enhances system
throughput, reduces latency, and ensures correct execution results.

### Implications of Dependency Distance on DAG Scheduling

When **dependency_distance** is large, **hints** and **dependency DAG** become less critical since transactions can be
optimistically executed in parallel with minimal risk of re-execution. However, when **dependency_distance** is small
(e.g., **dependency_distance ≤ 3**), the likelihood of conflicts increases, leading to more frequent re-execution of
affected transactions. If dependencies are not dynamically updated, conflicting transactions may require over 10
execution attempts to reach confirmation. Implementing dynamic updates to the **dependency DAG** can significantly
reduce the number of retries:

- **Execution phase updates**: Reducing dependencies here lowers retries to about 5.
- **Validation phase updates**: Further reduces retries to around 3.
- **Finality phase updates**: Limits retries to at most 2.

Since both dependency updates and transaction re-executions introduce overhead, a balance must be struck, with
**dependency_distance** serving as the primary optimization metric.

When conflicts are frequent, **hints** accuracy becomes particularly important. If **hints** are highly accurate, even
an execution-phase dependency removal strategy can prevent conflicts, ensuring the fastest scheduling speed. However,
inaccurate **hints** lead to frequent **dependency DAG** updates, degrading performance.

While the **Task Group** mechanism improves performance, to reduce reliance on **hints** accuracy, only transactions
with **dependency_distance = 1** are grouped into **Task Groups**. This ensures that even if **hints** are inaccurate,
sequential execution of dependent transactions remains conflict-free, minimizing performance loss.

Overall, compared to **Block-STM**, **grevm2.0** offers no significant advantage in low-conflict scenarios. However, in
high-conflict environments, **grevm2.0** effectively reduces retries (lowering CPU consumption) and leverages
**dependency_distance** for further optimizations when **hints** are reliable. By incorporating dynamic dependency
updates and **Task Groups**, **grevm2.0** excels in high-conflict scenarios, further improving system performance.

### Validation Scheduling

In **grevm2.0**, **Validation** scheduling is slower than in **Block-STM** for two key reasons.

The first is straightforward: **grevm2.0** does not execute transactions strictly in order of their indices, but
**Validation** must proceed in ascending order. This means **Validation** often has to wait for sequentially executed
transactions to complete, causing scheduling delays.

The second reason relates to **Block-STM**'s commonly overlooked optimization for **Validation**. **Block-STM**
introduces a **write_new_locations** flag for each transaction to indicate whether it has written to a new memory
location. If a transaction encounters a conflict, **validation_idx** advances directly to **tx + 1**, meaning it does
not need to wait for the conflicting transaction to be re-executed before moving forward. If, after re-execution,
**write_new_locations = false**, only a single **Validation** task needs to be processed, and **validation_idx** remains
unchanged. It is only advanced when **write_new_locations = true**. Since the probability of this flag being **true**
after a retry is low, **Block-STM** accelerates validation for non-conflicting transactions while avoiding unnecessary
re-validation.

Since **Validation** is a prerequisite for **finality**, and features like **remove dynamic dependency**,
**async-commit**, **miner**, and **self-destruct** all depend on **finality**, optimizing **grevm2.0**'s **Validation**
speed is crucial.

A simple approach would be to validate transactions immediately after execution, following **Block-STM**'s strategy for
checking the **write set** of earlier transactions. However, this method would also require scanning the **read set** of
later transactions, which is computationally expensive. Checking the **write set** of earlier transactions only involves
verifying the latest written location, whereas checking the **read set** of later transactions requires examining all
subsequent transactions to ensure they accessed the correct data—an inefficient process.

A more effective solution is to validate a transaction's **write set** and, if its modifications exceed the predicted
range of **hints**, handle those out-of-range transactions using the standard validation process. However, for
transactions within the **hints** range, their read/write sets do not require validation. Thus, optimizing
**grevm2.0**'s **Validation** speed depends heavily on **hints** accuracy. By dynamically refining **hints** and
improving the validation mechanism, the system can significantly boost performance while maintaining correctness.

## Authors

- [https://github.com/AshinGau](https://github.com/AshinGau)
- [https://github.com/nekomoto911](https://github.com/nekomoto911)
- [https://github.com/Richard](https://github.com/Richard19960401)
- [https://github.com/stumble](https://github.com/stumble)
