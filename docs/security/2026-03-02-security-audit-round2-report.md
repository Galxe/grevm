# Security Audit Report — grevm (Round 2)

**Date:** 2026-03-02
**Scope:** All Rust source in `src/` (~3000 LOC)
**Language:** Rust | **Framework:** revm v29, Block-STM
**Repository:** `Galxe/grevm` (branch: `security-audit-fixes-round-2`, commit `87322cb`)
**Prior Audit:** [Round 1 — 2026-02-26](./2026-02-26-security-audit-report.md) (19 findings, 16 fixed)

---

## Summary

| Severity | Count |
|----------|-------|
| HIGH     | 2 |
| MEDIUM   | 3 |
| LOW      | 3 |
| INFO     | 2 |
| **Total** | **10** |

> [!NOTE]
> This Round 2 audit was performed against the codebase **after** Round 1 fixes were applied.
> Findings from Round 1 that were fixed or acknowledged are not re-listed.
> Findings that would clearly be rejected based on Round 1 reviewer feedback (e.g., algorithmic invariant panics,
> Block-STM design-level lock ordering, consensus-layer bounds) have been filtered out.

---

## HIGH Severity (2)

### GREVM-R2-001: `UnsafeCell` Safety Invariant Not Enforced by the Type System

**Files:** `src/scheduler.rs:268, 454, 563-564`, `src/async_commit.rs:30, 54-56`

**Issue:** The Round 1 fix correctly replaced raw pointer casts with `UnsafeCell`, but the safety invariant — *"only the commit thread mutates state"* — is documented in comments rather than enforced by Rust's type system. The `state_mut()` method in both `Scheduler` (line 563) and `StateAsyncCommit` (line 54) returns `&mut ParallelState<DB>` from a shared reference, relying solely on programmer discipline:

```rust
// scheduler.rs:563
fn state_mut(&self) -> &mut ParallelState<DB> {
    unsafe { &mut *self.state.get() }
}
```

The `Scheduler` struct manually implements `Sync` (line 287), and `StateAsyncCommit` manually implements `Send` (line 38). Both carry SAFETY comments but no compile-time enforcement exists to prevent a future code change from calling `state_mut()` from a worker thread.

**Impact:** A future refactor that accidentally calls `state_mut()` from a worker thread would silently introduce a data race — the exact class of bug that Round 1 fixed. The `unsafe impl Sync` makes this invisible to the compiler. This is a **latent UB vector** that current code does not trigger but is one careless commit away from activating.

**Recommendation:** Encapsulate the write-side access into a dedicated wrapper type that can only be constructed by the commit thread:

```rust
/// Only constructible within the commit thread's scope.
/// Guarantees exclusive mutable access to ParallelState.
struct CommitGuard<'a, DB: DatabaseRef> {
    state: &'a UnsafeCell<ParallelState<DB>>,
}

impl<'a, DB: DatabaseRef> CommitGuard<'a, DB> {
    fn state_mut(&mut self) -> &mut ParallelState<DB> {
        // SAFETY: CommitGuard is only created for the commit thread.
        unsafe { &mut *self.state.get() }
    }
}
```

This makes it structurally impossible for worker threads to obtain mutable access.

---

### GREVM-R2-002: Commit Continues After Nonce Validation Failure

**File:** `src/async_commit.rs:82-138`

**Issue:** In `StateAsyncCommit::commit()`, when the nonce check fails (lines 98-116), the method sets `self.commit_result = Err(...)` but **does not return early**. Execution continues to:
1. Push the result to `self.results` (line 127)
2. Call `self.state_mut().commit(state)` (line 128) — applying the invalid transaction's state changes
3. Call `self.state_mut().increment_balances(...)` (line 137) — applying miner reward for the invalid tx

```rust
pub(crate) fn commit(&mut self, txid: TxId, tx_env: &TxEnv, result_and_state: ResultAndState) {
    // ... nonce check sets self.commit_result = Err(...) but does NOT return ...
    self.results.push(result);
    self.state_mut().commit(state);               // ← state applied even on nonce failure
    assert!(self.state_mut().increment_balances(vec![(self.coinbase, lazy_reward)]).is_ok());
}
```

The error is only checked later in `async_commit()` (scheduler.rs:415) where `commiter.commit_result().is_err()` triggers an abort. However, by that point the corrupted state has already been committed.

**Impact:** If a nonce mismatch is detected, the invalid transaction's state changes are applied to `ParallelState` before the abort. While the overall execution will abort and the result is discarded, this creates a window where `ParallelState` contains corrupted data that concurrent reader threads might observe via `DatabaseRef::basic_ref()`. In the worst case, a subsequent transaction's validation (reading from the DashMap cache) could see the invalid state.

**Recommendation:** Return immediately after setting the error:

```rust
Ordering::Greater => {
    self.commit_result = Err(GrevmError { ... });
    return;  // ← do not apply state
}
Ordering::Less => {
    self.commit_result = Err(GrevmError { ... });
    return;  // ← do not apply state
}
```

---

## MEDIUM Severity (3)

### GREVM-R2-003: `fork_join_util` Produces Empty Partitions Without Guard

**File:** `src/lib.rs:178-193`

**Issue:** When `num_elements < parallel_cnt`, some partitions will have `start_pos == end_pos` (empty range). The closure `f` is still invoked for these empty partitions. While current callers handle this correctly (the loop body `for pos in start_pos..end_pos` simply doesn't execute), this is a fragile API contract:

```rust
pub fn fork_join_util<'scope, F>(num_elements: usize, num_partitions: Option<usize>, f: F)
where
    F: Fn(usize, usize, usize) + Send + Sync + 'scope,
{
    let parallel_cnt = num_partitions.unwrap_or(*CONCURRENT_LEVEL);
    // When num_elements=3 and parallel_cnt=8, 5 threads get start_pos == end_pos
    let remaining = num_elements % parallel_cnt;
    let chunk_size = num_elements / parallel_cnt;  // = 0 when num_elements < parallel_cnt
    (0..parallel_cnt).into_par_iter().for_each(|index| { ... });
}
```

More critically, when `num_elements == 0`, all threads get empty ranges but `parallel_cnt` rayon tasks are still spawned, wasting thread pool time.

**Impact:** No correctness bug today, but unnecessary thread pool contention when processing small or empty batches. Future callers of this public API could introduce bugs if they assume `start_pos < end_pos`.

**Recommendation:** Early-return when `num_elements == 0` and cap `parallel_cnt` at `num_elements`:

```rust
if num_elements == 0 { return; }
let parallel_cnt = min(num_partitions.unwrap_or(*CONCURRENT_LEVEL), num_elements);
```

---

### GREVM-R2-004: `clear_destructed_entry` Full Table Scan is O(n) on MVMemory Size

**File:** `src/storage.rs:322-334`

**Issue:** When a self-destructed account is encountered and `commit_idx == current_tx.txid`, the method `clear_destructed_entry()` performs a **full iteration** over the entire `DashMap`:

```rust
fn clear_destructed_entry(&self, account: Address) {
    let current_tx = self.current_tx.txid;
    for mut entry in self.mv_memory.iter_mut() {
        let destructed = match entry.key() {
            LocationAndType::Basic(address) => *address == account,
            LocationAndType::Storage(address, _) => *address == account,
            LocationAndType::Code(address) => *address == account,
        };
        if destructed {
            *entry.value_mut() = entry.value_mut().split_off(&current_tx);
        }
    }
}
```

`iter_mut()` on a `DashMap` acquires each shard's write lock sequentially, blocking all other threads that need to read or write any entry in that shard. If MVMemory contains thousands of entries (one per storage slot per account per transaction), this becomes a significant contention point during parallel execution.

**Impact:** Performance degradation and potential thread starvation when self-destruct operations occur in blocks with high storage slot counts. A malicious contract could exploit `SELFDESTRUCT` to force this worst-case scan, degrading parallel throughput to near-sequential.

**Recommendation:** Maintain a secondary index `DashMap<Address, HashSet<LocationAndType>>` to enable O(1) lookup of entries by address, avoiding the full table scan. Alternatively, prefix-scan the DashMap only for the target address keys.

---

### GREVM-R2-005: `tx_dependency.print()` Clones and Locks All State Under `tracing::debug!`

**File:** `src/tx_dependency.rs:152-163`

**Issue:** The `print()` method acquires every lock in both `dependent_state` and `affect_txs` vectors to clone the entire dependency graph for a debug log:

```rust
pub(crate) fn print(&self) {
    let dependent_tx: Vec<(TxId, DependentState)> =
        self.dependent_state.iter().map(|dep| dep.lock().clone()).enumerate().collect();
    let affect_txs: Vec<(TxId, HashSet<TxId>)> =
        self.affect_txs.iter().map(|affects| affects.lock().clone()).enumerate().collect();
    tracing::debug!(...);
}
```

This unconditionally acquires `2 * num_txs` locks and clones all data, even if the `debug` log level is disabled. For a block with 1000 transactions, this is 2000 lock acquisitions plus memory allocation.

**Impact:** If `print()` is called during hot execution paths (even with debug logging disabled), the lock acquisitions cause contention. If debug logging is enabled in production, the serialization of all locks could cause visible latency spikes.

**Recommendation:** Gate the expensive work behind a log-level check:

```rust
pub(crate) fn print(&self) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    // ... existing code ...
}
```

Or use `tracing`'s lazy evaluation with `tracing::debug!(...)` and deferred formatting.

---

## LOW Severity (3)

### GREVM-R2-006: `DisjointVec::set` Has No Bounds Check

**File:** `src/storage.rs:32-34`

**Issue:** `DisjointVec::set()` performs an unchecked index operation in the unsafe block:

```rust
unsafe fn set(&self, index: usize, value: T) {
    unsafe { (&mut (*self.0.get()))[index] = value };
}
```

While `fork_join_util` guarantees that indices are within bounds, the `DisjointVec` type itself provides no bounds validation. If used incorrectly (e.g., with a miscomputed partition range), this would silently write out of bounds — classic buffer overflow UB.

**Recommendation:** Add a debug assertion:

```rust
unsafe fn set(&self, index: usize, value: T) {
    debug_assert!(index < (*self.0.get()).len(), "DisjointVec: index out of bounds");
    unsafe { (&mut (*self.0.get()))[index] = value };
}
```

---

### GREVM-R2-007: `IdentityHasher::write()` Panics with `unreachable!()`

**File:** `src/parallel_state.rs:436-437`

**Issue:** The `IdentityHasher` only supports `write_u64` and `write_usize`. The generic `write(&[u8])` method is `unreachable!()`:

```rust
fn write(&mut self, _: &[u8]) {
    unreachable!()
}
```

This is used by `DashMap<u64, B256, BuildIdentityHasher>` for `block_hashes`. If any code path (including third-party updates to `DashMap`) ever calls the generic `write()` instead of `write_u64()`, the node panics.

**Recommendation:** Implement a fallback that processes the byte slice:

```rust
fn write(&mut self, bytes: &[u8]) {
    // Fallback: hash the bytes to a u64. Should not normally be called.
    debug_assert!(false, "IdentityHasher::write called unexpectedly");
    let mut val = 0u64;
    for chunk in bytes.chunks(8) {
        let mut arr = [0u8; 8];
        arr[..chunk.len()].copy_from_slice(chunk);
        val ^= u64::from_ne_bytes(arr);
    }
    self.0 = val;
}
```

---

### GREVM-R2-008: `parallel_apply_transitions_and_create_reverts` Clones Transitions via `get().cloned()`

**File:** `src/storage.rs:77-96`

**Issue:** Inside the fork-join closure, each transition is looked up and cloned:

```rust
let transition = transitions.get(&address).cloned().unwrap();
```

Since `transitions` is a consumed `TransitionState` (owned `HashMap`), the data could be moved rather than cloned. The `cloned()` call performs a deep copy of every `TransitionAccount`, including its `StorageWithOriginalValues` (which can be large for contracts with many storage slots).

**Impact:** Unnecessary memory allocation and copying proportional to the total storage changes in the block. For blocks with heavy DEX activity (thousands of storage slot changes), this canlead to measurable overhead.

**Recommendation:** Restructure to consume the `HashMap` into a `Vec<(Address, TransitionAccount)>` before the fork-join, allowing each partition to move values rather than clone:

```rust
let transitions_vec: Vec<(Address, TransitionAccount)> = transitions.transitions.into_iter().collect();
fork_join_util(transitions_vec.len(), None, |start, end, _| {
    for pos in start..end {
        let (address, transition) = &transitions_vec[pos]; // or take ownership via UnsafeCell
        // ...
    }
});
```

---

## INFO (2)

### GREVM-R2-INFO-001: `release` Profile Sets `panic = "abort"`

**File:** `Cargo.toml:63`

**Issue:** The release profile uses `panic = "abort"`:
```toml
[profile.release]
panic = "abort"
```

Combined with the remaining `panic!`/`assert!` calls (acknowledged as intentional in Round 1), this means any assertion failure immediately terminates the process without unwinding. While this is a deliberate design choice for a blockchain node (preventing corrupted state propagation), it also means that `Drop` implementations for open file handles, network connections, or database cursors will **not** be called on panic.

---

### GREVM-R2-INFO-002: Forked Dependencies on Private Branch

**File:** `Cargo.toml:10-16, 23`

**Issue:** All `revm` crates and `alloy-evm` are sourced from `Galxe/revm` (branch `v29.0.1-gravity`) and `Galxe/alloy-evm` (branch `v0.21.3-gravity`). These are forks of upstream crates.

**Impact:** Security patches applied to upstream `revm` or `alloy-evm` will not automatically propagate to these forks. If an upstream vulnerability is discovered and patched, the grevm project must manually merge or cherry-pick the fix. This creates a window of exposure between upstream fix and fork update.

**Recommendation:** Establish a documented process for periodically syncing with upstream releases. Consider subscribing to upstream security advisories. Track the delta between the fork and upstream to minimize merge conflicts.

---

## Appendix: Methodology

This audit was conducted using the following approach, guided by the `security-auditor`, `security-scanning-security-sast`, `threat-modeling-expert`, and `rust-pro` skills:

1. **Full Source Review:** Manual line-by-line review of all 8 source files in `src/` (~3000 LOC)
2. **Unsafe Code Audit:** Enumerated all `unsafe` blocks, `UnsafeCell` usage, and manual `Sync`/`Send` implementations
3. **Concurrency Analysis:** Traced all lock acquisition patterns, atomic orderings, and thread communication channels
4. **Data Flow Analysis:** Followed transaction state through the execution → validation → finality → commit pipeline
5. **Dependency Review:** Assessed Cargo.toml for supply chain risks
6. **Round 1 Follow-up:** Verified that Round 1 fixes are correctly implemented; identified new findings at the same code locations where fixes were applied

### Filtering Criteria

Per the reviewer's guidance from Round 1, the following categories of findings were **excluded** from this report:

- **Algorithmic invariant panics:** `panic!`/`assert!` in the commit/execution/validation pipeline are intentional guards for severe consensus invariants (per GREVM-007 review)
- **Block-STM design-level race conditions:** Where the reviewer explained that the algorithm's logical timestamp or monotonic ordering guarantees correctness despite apparent TOCTOU patterns (per GREVM-004/-006 review)
- **Lock ordering "deadlocks":** Where business logic guarantees `dep_id < txid`, making lock cycles impossible (per GREVM-005 review)
- **Consensus-layer bounds:** Where the block gas limit already constrains values like transaction count (per GREVM-016 review)
- **Convenience/style issues:** Such as redundant fields (`block_size` vs `txs.len()`) with no actual risk (per GREVM-015 review)
- **Type-safe operations that cannot fail:** Such as `[12..]` on `FixedBytes<32>` (per GREVM-012 review)
