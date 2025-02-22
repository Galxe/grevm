# Grevm 2.0

## **TL;DR – Highlights of Grevm 2.0**

- **Grevm 2.0 achieves near-optimal performance in low-contention scenarios**, matching Block-STM with **16.57
  gigagas/s** for Uniswap workloads and outperforming it with **95% less CPU usage** in inherently non-parallelizable
  cases by **20–30%**, achieving performance close to sequential execution.
- **Breaks Grevm 1.0’s limitations in handling highly dependent transactions**, delivering a **5.5× throughput
  increase** to **2.96 gigagas/s** in **30%-hot-ratio hybrid workloads** by minimizing re-executions through **DAG-based
  scheduling** and **Task Groups**.
- **Parallel State module enhances state storage efficiency**, introducing **asynchronous execution result bundling**
  that amortizes **~30ms post-execution overhead**, while seamlessly handling **miner rewards and self-destruct opcode**
  without the performance penalties of sequential fallbacks.
- **In-depth analysis of optimistic parallel execution** reveals the **underestimated efficiency of Block-STM** and the
  strength of **optimistic parallelism**, providing new insights into high-performance transaction execution.

## Abstract

Grevm 2.0 integrates Block-STM with an execution scheduling mechanism based on **Task Groups** and a **Data Dependency
Directed Acyclic Graph (DAG)**, derived from simulated transaction execution results. Unlike Block-STM, which schedules
tasks solely based on the lowest index, Grevm 2.0 dynamically schedules execution based on the DAG, prioritizing
lower-index transactions with no dependencies for parallel execution while grouping strongly dependent transactions into
task groups. Within each task group, transactions execute sequentially within the same thread to minimize re-executions,
reduce scheduling overhead, and optimize CPU utilization. This design significantly reduces transaction re-executions in
high-conflict scenarios and maximizes parallelism by enabling a parallel execution order that mirrors an optimally
reordered sequence—without altering the original transaction order.

Our benchmark results demonstrate that, compared to Grevm 1.0, Grevm 2.0 achieves **5.5× higher throughput** for a
**30%-hot-ratio hybrid workload** (Uniswap, ERC20, and raw transfers), reaching **2.96 gigagas/s**. Additionally, while
Grevm 2.0 matches Block-STM’s performance for low-conflict workloads, it outperforms Block-STM in extreme cases by
maintaining the same performance as sequential execution—avoiding the 20–30% slowdown observed in Block-STM—while using
**95%** less CPU usage.

From an engineering perspective, Grevm 2.0 introduces **parallel state**, an asynchronous state storage mechanism that
bundles final execution results in parallel. This approach elegantly addresses challenges related to miner rewards and
the **self-destruct** opcode without compromising correctness or imposing a performance penalty when falling back to
sequential execution.

In this report, we first present the design of Grevm 2.0 and its benchmark results. Then, we share rarely discussed data
highlighting Block-STM's impressive optimistic execution efficiency, which we discovered during our experiments and
which shaped our key design choices.

## Algorithm Design

Grevm 2.0 consists of three core modules: **Dependency Manager (DAG Manager)**, **Execution Scheduler**, and **Parallel
State Storage**. It employs a DAG-driven task scheduling mechanism to:

1. Dynamically maintain the data dependency DAG using using a **selective update strategy**.
2. Group adjacent dependent transactions into **task groups**.
3. Execute task groups and the lowest-index transactions with no dependencies (i.e., out-degree of 0).

![image.png](images/g2design.png)

### Dependency Manager and Execution Scheduler

The **Dependency Manager** tracks and resolves transaction dependencies during parallel execution. Like in Grevm 1.0,
transactions are represented in a **Directed Acyclic Graph (DAG)**:

- **Nodes represent transactions**, identified by unique indices. Let $T_i$ be a transaction with index $i$.
- **Edges denote dependencies**, where an edge from $T_j$ to $T_i$ exists if $T_i$ writes data that $T_j$ reads,
  indicating a read-after-write dependency. Notably, $j$ is always strictly less than $i$.

Before execution, dependencies are inferred using **hints**—speculated read/write sets obtained from static analysis or
simulation (executing transactions on the last committed state).

The **Execution Scheduler** manages both transaction **execution** and **validation**, following this workflow:

1. **Parallel Execution**: The scheduler selects and executes transactions with the smallest index from the DAG that
   have no dependencies (out-degree = 0). For a task group, its index is the smallest among all grouped transactions.
2. **Validation**: After execution, transactions enter a **pending** validation phase. The scheduler checks whether the
   read set of the smallest-index pending transaction has changed:
   - If unchanged, the transaction moves to the **unconfirmed** state. Consecutive unconfirmed transactions transition
     into **finality**, starting from the lowest index.
   - If changed, the transaction is marked as a **conflict**, reinserted into the DAG, and its dependencies are updated
     before re-execution.

When consecutive transactions have dependencies, they are grouped into a **Task Group**, which is scheduled and executed
sequentially within the same thread. For example, if $tx_2$ depends on $tx_3$, $tx_2$ must finish before $tx_3$ begins.
By executing them sequentially within a thread, **task groups reduce task switching, re-execution, and scheduling
overhead**, making them highly effective in **high-conflict workloads** like NFT minting and DeFi transactions.

In the worst-case scenario, where all transactions form a dependency chain, **task group execution remains as efficient
as serial execution**. This design **balances optimistic parallelism with efficient CPU utilization**, while maintaining
the simplicity of Block-STM's scheduling logic—avoiding excessive complexity that could degrade performance for simple,
fast-executing transactions, like ERC20 and raw transfers.

### Selective Dependency DAG Update Strategy

Grevm 2.0 follows a **selective dependency strategy**, adding only the most necessary edges to minimize DAG
modifications. Dependencies are added in three scenarios:

1. **Misestimated Reads**: When a transaction reads estimated data that later proves changed, instead of aborting
   immediately and marking it as dependent, Grevm 2.0 completes execution, analyzes the actual read-write set, and adds
   **only the most significant dependency**—specifically, the highest-index transaction it depends on.
2. **Reads from Miner or Self-Destructed Accounts**: If a transaction $T_j$ reads from a **miner account** or a
   **self-destructed account** while the parallel state for $T_{j-1}$ has not yet been committed, instead of linking to
   all prior transactions, Grevm 2.0 adds only $T_{j-1}$ as a dependency, reducing redundant edges.
3. **Validation Phase Conflicts**: If a transaction's read set changes during validation, Grevm 2.0 recalculates
   dependencies and adds only the **most recent transaction** as a dependency. Specifically, if $T_i$ detects a conflict
   with multiple transactions, it finds the one with the **largest index $k$** and adds a dependency edge from $T_k$ to
   $T_i$.

Efficient dependency removal is also crucial for balancing scheduling speed and minimizing re-executions. Dependencies
can be removed at different stages, each with trade-offs:

- **After Execution**: Enables faster scheduling of subsequent transactions but increases the risk of re-execution if
  dependencies were misestimated.
- **After Validation**: Delays scheduling slightly but reduces unnecessary re-executions.
- **After Finality**: Eliminates all re-execution risks but introduces the longest scheduling delay.

Grevm 2.0 adopts a **hybrid Execution + Finality strategy** to achieve an optimal balance:

- **Pre-execution dependencies** (inferred before execution) are removed immediately after execution to maximize
  scheduling throughput.
- **Dynamically detected dependencies** (discovered during execution or validation) are removed only after finality to
  ensure correctness and minimize re-execution.

This strategy ensures efficient scheduling when dependency hints are accurate while mitigating re-execution overhead
when hints are unreliable. Empirical results demonstrate that this hybrid approach effectively balances execution
throughput and re-execution probability, delivering robust performance across diverse workload conditions.

### Parallel State Storage

![image.png](images/parallel_state.png)

The **Storage** module implements **DatabaseRef** and provides two key functionalities: **Parallel State** and
**Multi-Version Memory (MvMemory)**. It introduces a **Parallel State** layer between EvmDB and MvMemory to enhance
performance.

After transaction $T_i$ completes execution, its results are stored in **MvMemory**. Once it reaches the **Finality**
state, an asynchronous task commits the results to **Parallel State**. When transaction $T_j$ accesses data, it reads
from **Parallel State** if dealing with a miner or self-destructed account; otherwise, it retrieves data from
**MvMemory**.

This design offers several advantages:

- **Amortized Result State Building**: Transactions continuously generate Reth-compatible result states, eliminating the
  **~30ms latency** of bundling them after full block execution.
- **Efficient State Management**: Since Parallel State implements all
  [EVM State](https://github.com/bluealloy/revm/blob/main/crates/database/src/states/state.rs) interfaces, transactions
  can retrieve the latest miner or self-destructed account state without relying entirely on MvMemory.

In cases where a transaction executes **self-destruct**, conflicts arise if later transactions attempt to access that
account. The self-destruct operation updates MvMemory's write set and marks the account as **self-destructed**. If a
subsequent transaction retrieves self-destructed account data from MvMemory, it must re-fetch the latest state from
Parallel State.

Parallel State's **commit ID** serves as a version number, ensuring that for transaction $j$, only data from version
$j-1$ is valid—otherwise, the transaction is marked as conflicting.

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

Compared to Grevm 1.0, Grevm 2.0 does not achieve the same level of speedup when transaction complexity is low. This is
expected, as DAG-based scheduling incurs higher overhead than 1.0’s partitioning algorithm. However, as transaction
complexity increases, Grevm 2.0’s performance matches and slightly surpasses 1.0. For instance, in the Uniswap swap test
with 100µs access latency, Grevm 2.0 achieves a **60.8× speedup**, compared to 1.0’s **58.06×**. Even in cases where 2.0
is less efficient than 1.0—such as raw and ERC20 transfers—its throughput still significantly exceeds **1 gigagas/s**,
making this trade-off well-suited for production workloads.

Like Grevm 1.0, Grevm 2.0 benefits from **asynchronous I/O**, enabled by parallel execution, further amplifying its
performance advantage over sequential execution.

Additionally, with **parallel state storage**, all tests now include overheads that were excluded in 1.0, such as state
bundling. This explains why the Grevm 1.0 benchmark results referenced here are **end-to-end execution time**, as
reported in Table 3 of the original paper.

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

In Grevm 1.0, the performance of high-contention transactions was constrained by their high interdependence. Grevm 2.0
significantly improves performance in high-conflict scenarios. With a **30% hot ratio**, Grevm 2.0 outperforms 1.0 in
all test cases except for ERC20 transfers with zero latency, where experimental variance is a factor. The most notable
improvement is in the **Hybrid test case**, where Grevm 2.0 achieves a **29× speedup over sequential execution**, which
is **5.55× over Grevm 1.0**, reaching a throughput of **2.96 gigagas/s**.

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
