# Security Audit Report — grevm

**Date:** 2026-02-26
**Scope:** All Rust source in `src/` (~3000 LOC)
**Language:** Rust | **Framework:** revm v29, Block-STM
**Repository:** `Galxe/grevm` (branch: `main`, commit `26b586c`)

---

## Summary

| Severity | Count | Status |
|----------|-------|--------|
| CRITICAL | 3 | **Fixed** (2026-02-27, branch `security-audit-fixes`) |
| HIGH     | 3 | **Fixed** |
| MEDIUM   | 5 | **Fixed** |
| LOW      | 5 | 3 Fixed, 2 Deferred |
| INFO     | 3 | 2 Fixed, 1 No action needed |
| **Total** | **19** | **16 Fixed, 3 Deferred** |

---

## CRITICAL Severity (3)

### GREVM-001: Undefined Behavior via `invalid_reference_casting`

**Files:** `src/scheduler.rs:551-555`, `src/async_commit.rs:40-44`, `src/utils.rs:42-44`, `src/hint.rs:123-124`, `src/storage.rs:53-64`
**Instances:** 7 total across 5 files

**Issue:** The codebase contains 7 instances where shared references (`&T`) are cast to mutable references (`&mut T`) via raw pointer transmutation, all suppressing the `invalid_reference_casting` lint:

```rust
#[allow(invalid_reference_casting)]
unsafe {
    &mut *(&self.state as *const ParallelState<DB> as *mut ParallelState<DB>)
}
```

This is instant **Undefined Behavior** under Rust's aliasing model (stacked borrows). The compiler may optimize away writes, reorder operations, or produce arbitrary behavior. Miri would flag every instance. The `invalid_reference_casting` lint exists precisely because this pattern is unsound.

**Impact:** Unpredictable behavior at any optimization level. Compiler upgrades could silently break correctness. State corruption, incorrect execution results, or crashes are all possible.

**Affected patterns:**
- `scheduler.rs:551-555` — `state_mut()` to get `&mut ParallelState` for fallback sequential
- `async_commit.rs:40-44` — `state_mut()` for commit thread to mutate `ParallelState`
- `utils.rs:42-44` — `ContinuousDetectSet::add()` mutates `index_flag: Vec<bool>`
- `hint.rs:123-124` — `parse_hints()` mutates `rw_set: Vec<RWSet>` from parallel threads
- `storage.rs:53-64` — `parallel_apply_transitions_and_create_reverts()` mutates `reverts`, `addresses`, `bundle_state` vectors from parallel threads

**Recommendation:** Replace all `&T → &mut T` casts with proper interior mutability:
- `ContinuousDetectSet::index_flag` → `Vec<AtomicBool>`
- `ParallelState` mutation → `UnsafeCell` with documented safety invariants, or refactor ownership
- `storage.rs` fork-join → partition data ownership per thread, or use `UnsafeCell`-based parallel slice
- `hint.rs` → partition `rw_set` slices per thread (each thread writes disjoint indices)

---

### GREVM-002: Data Race on `ContinuousDetectSet::index_flag`

**File:** `src/utils.rs:40-48`

**Issue:** Multiple worker threads call `ContinuousDetectSet::add(index)` concurrently. The method:
1. Reads `self.index_flag[index]` (line 41) — unsynchronized read
2. Casts to `&mut Vec<bool>` via UB (line 43-44)
3. Writes `index_flag[index] = true` (line 45) — unsynchronized write

Even if each thread writes to a different index, `Vec<bool>` is not designed for concurrent access. The `Vec` metadata (length, capacity, pointer) could be corrupted if the Vec is ever reallocated. Additionally, `check_continuous()` (line 47) reads the flag vector concurrently with writes from other threads — there is no happens-before relationship.

```rust
pub(crate) fn add(&self, index: usize) {
    if !self.index_flag[index] {                    // unsynchronized read
        #[allow(invalid_reference_casting)]
        let index_flag =
            unsafe { &mut *(&self.index_flag as *const Vec<bool> as *mut Vec<bool>) };
        index_flag[index] = true;                   // unsynchronized write
        self.num_index.fetch_add(1, Ordering::Relaxed);
        self.check_continuous();
    }
}
```

**Impact:** Torn reads in `check_continuous()` could cause `continuous_idx` to skip indices or stall, breaking the finality pipeline. Workers could miss validation tasks or finalize out of order.

**Recommendation:** Replace `Vec<bool>` with `Vec<AtomicBool>`:
```rust
index_flag: Vec<AtomicBool>,
// In add():
if !self.index_flag[index].swap(true, Ordering::Release) {
    self.num_index.fetch_add(1, Ordering::Relaxed);
    self.check_continuous();
}
```

---

### GREVM-003: Unsound Concurrent Mutation of `ParallelState` via Async Commit

**File:** `src/async_commit.rs:40-44, 65-121`

**Issue:** The `StateAsyncCommit` holds an `&ParallelState<DB>` shared reference but mutates it through `state_mut()` (line 40-44) which casts `&T` to `&mut T`. The commit thread calls:
- `self.state_mut().commit(state)` (line 111) — writes to `transition_state`
- `self.state_mut().increment_balances(...)` (line 120) — writes to account balances

Meanwhile, worker threads concurrently read from the same `ParallelState` via `self.db.basic_ref()` in `CacheDB` (storage.rs). The `transition_state` field inside `ParallelState` (a `TransitionState` from revm) is **not thread-safe** — it uses `HashMap` internally.

**Impact:** Concurrent read/write of `HashMap` causes data races: corrupted hash table, infinite loops in probe sequences, memory corruption. The nonce check at line 73 (`self.state.basic_ref(tx_env.caller)`) reads while `commit()` at line 111 writes.

**Recommendation:** Split `ParallelState` into read-only and write-only portions. The commit thread should own a separate mutable handle (e.g., `Mutex<TransitionState>` or channel-based approach). Worker threads should only access the immutable database reference.

---

## HIGH Severity (3)

### GREVM-004: TOCTOU Race in `async_finality`

**File:** `src/scheduler.rs:332-391`

**Issue:** The `async_finality` method checks transaction status at line 341 (acquiring the lock), then drops the lock implicitly before proceeding to check timestamps (lines 344-354). It then re-acquires the lock at line 355 to set `Finality` status — but the status may have changed between the two lock acquisitions:

```rust
// First lock acquisition (line 341)
if self.tx_states[finality_idx].lock().status != TransactionStatus::Unconfirmed {
    break;
}
// Lock dropped here — status can change!
lower_ts = max(lower_ts, ...);
if self.scheduler_ctx.unconfirmed_ts[finality_idx].load(...) <= lower_ts {
    break;
}
// Second lock acquisition (line 355) — status may no longer be Unconfirmed
let mut tx_state = self.tx_states[finality_idx].lock();
tx_state.status = TransactionStatus::Finality;
```

A worker thread can change the status to `Conflict` (triggering re-execution) between lines 341 and 355.

**Impact:** A transaction could be marked `Finality` while it's actually in `Conflict` state and being re-executed. This would commit stale/incorrect results.

**Recommendation:** Hold the lock across the entire check-and-set sequence:
```rust
let mut tx_state = self.tx_states[finality_idx].lock();
if tx_state.status != TransactionStatus::Unconfirmed { break; }
// ... timestamp checks ...
tx_state.status = TransactionStatus::Finality;
```

---

### GREVM-005: Potential Deadlock in `TxDependency::add()`

**File:** `src/tx_dependency.rs:125-150`

**Issue:** `add()` acquires three locks in sequence:
1. `self.affect_txs[dep_id].lock()` (line 127)
2. `self.dependent_state[dep_id].lock()` (line 128)
3. `self.dependent_state[txid].lock()` (line 129)

Meanwhile, `remove()` (line 72-94) acquires `self.affect_txs[txid].lock()` then `self.dependent_state[tx].lock()`. If thread A calls `add(txid=5, dep_id=3)` while thread B calls `remove(txid=3)`:
- Thread A holds `affect_txs[3]`, waits for `dependent_state[3]`
- Thread B holds `dependent_state[5]` (from iterating affects), waits for `affect_txs[3]`

This creates a lock cycle → deadlock.

**Impact:** Consensus halt — the block execution thread pool deadlocks, blocking the entire chain.

**Recommendation:** Enforce a consistent global lock ordering (always acquire lower index first), or restructure to avoid holding multiple locks simultaneously.

---

### GREVM-006: `next_validation_idx()` Double-Increment Race

**File:** `src/scheduler.rs:237-246`

**Issue:** `next_validation_idx()` performs a non-atomic check-then-increment:

```rust
fn next_validation_idx(&self, executing_idx: usize) -> Option<usize> {
    let validation_idx = self.validation_idx.load(Ordering::Acquire);  // read
    if validation_idx < executing_idx && validation_idx < self.executed_set.continuous_idx() {
        let validation_idx = self.validation_idx.fetch_add(1, Ordering::AcqRel);  // increment
        // ^ This may return a DIFFERENT value than what was checked above
        if validation_idx < self.num_txs {
            return Some(validation_idx);
        }
    }
    None
}
```

Between the `load` and `fetch_add`, another thread may have already incremented `validation_idx` beyond `executing_idx`. The `fetch_add` will still succeed, returning a value that may violate the original guard condition. Two threads can also both pass the guard and both increment, causing validation to skip ahead.

**Impact:** Transactions may be validated before they've been executed (if `validation_idx` jumps past `executed_set.continuous_idx()`), or duplicate validation work is performed.

**Recommendation:** Use `compare_exchange` in a loop instead of separate load + fetch_add:
```rust
loop {
    let idx = self.validation_idx.load(Ordering::Acquire);
    if idx >= executing_idx || idx >= self.executed_set.continuous_idx() {
        return None;
    }
    if self.validation_idx.compare_exchange(idx, idx + 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        return Some(idx);
    }
}
```

---

## MEDIUM Severity (5)

### GREVM-007: `panic!()` in Production Commit Path

**File:** `src/async_commit.rs:78, 120`
**Also:** `src/scheduler.rs:402, 530, 618, 757-758, 762, 765`

**Issue:** Multiple `panic!`/`assert!`/`unwrap()` calls exist in production code paths:
- `async_commit.rs:78` — `assert_eq!(change.info.nonce, expect + 1)` during commit
- `async_commit.rs:120` — `assert!(self.state_mut().increment_balances(...).is_ok())`
- `scheduler.rs:402` — `panic!("Commit error tx: {}", commit_idx)`
- `scheduler.rs:530` — `panic!("Wrong abort transaction")`
- `scheduler.rs:618` — `panic!("Inconsistent incarnation when execution")`

**Impact:** Any of these assertions failing will crash the node. In a blockchain context, a deterministic panic on a specific block would halt all nodes running this block, causing a chain halt.

**Recommendation:** Replace panics with error returns (`GrevmError`) and propagate to the caller. The scheduler should abort gracefully and fall back to sequential execution.

---

### GREVM-008: Environment Variable Parsing with `unwrap()` at Runtime

**File:** `src/scheduler.rs:397, 435`

**Issue:** Environment variables are parsed with `unwrap()`:
```rust
// line 397
let async_commit_state = std::env::var("ASYNC_COMMIT_STATE").map_or(true, |s| s.parse().unwrap());
// line 435
std::env::var("GREVM_CONCURRENT_LEVEL").map_or(*CONCURRENT_LEVEL, |s| s.parse().unwrap()),
```

**Impact:** Setting `ASYNC_COMMIT_STATE=abc` or `GREVM_CONCURRENT_LEVEL=xyz` crashes the node.

**Recommendation:** Use `parse().unwrap_or(default)` or `parse().ok()`.

---

### GREVM-009: `get_contract_type()` Hardcoded to Always Return ERC20

**File:** `src/hint.rs:284-287`

**Issue:**
```rust
fn get_contract_type(_contract_address: Address, _data: &Bytes) -> ContractType {
    // TODO(gaoxin): Parse the correct contract type to determined how to handle call data
    ContractType::ERC20
}
```

Every contract call is treated as an ERC20 token interaction for dependency analysis.

**Impact:** Non-ERC20 contracts will have incorrect read/write set hints, causing either:
- False dependencies → unnecessary serialization (performance loss)
- Missing dependencies → missed conflicts → **incorrect parallel execution results**

The second case is the security concern: if the hint incorrectly models storage slots, the DAG may allow parallel execution of conflicting transactions.

**Recommendation:** Return `ContractType::UNKNOWN` for unrecognized contracts (making hints conservative), or remove the hint system for non-ERC20 contracts.

---

### GREVM-010: `println!()` Debug Output in Production Code

**File:** `src/scheduler.rs:378-388`

**Issue:** The `async_finality` method uses `println!()` for debug output when stuck for >8 seconds:
```rust
println!(
    "stuck..., block_number: {}, finality_idx: {}, validation_idx: {}, execution_idx: {}",
    ...
);
println!("transaction status: {:?}", status);
```

**Impact:** In production, this pollutes stdout and could leak internal execution state. If stdout is buffered or redirected to a file, it could cause I/O blocking.

**Recommendation:** Use `tracing::warn!()` or `log::warn!()` with structured fields, gated behind a log level.

---

### GREVM-011: `fork_join_util` Parallel Mutation via Unsound Unsafe

**File:** `src/storage.rs:52-84`

**Issue:** `parallel_apply_transitions_and_create_reverts` uses the same `&T → &mut T` UB pattern to mutate `reverts`, `addresses`, and `bundle_state` vectors from parallel threads. While each thread writes to disjoint indices (partitioned by `start_pos..end_pos`), the unsafe cast is still UB because:
1. Multiple `&mut` references to the same `Vec` exist simultaneously
2. The compiler assumes `&mut` is unique — optimizations may break this

**Impact:** Same as GREVM-001. The disjoint-index pattern is safe in practice but unsound in theory. A compiler upgrade could break it.

**Recommendation:** Use `std::cell::UnsafeCell` wrapping the vectors with a documented safety comment explaining the disjoint-access invariant. Or use `split_at_mut()` to get non-overlapping mutable slices.

---

## LOW Severity (5)

### GREVM-012: Missing Bounds Check in `hint.rs` Parameter Extraction

**File:** `src/hint.rs:231-232, 250-254`
**Issue:** `parameters[0].as_slice()[12..]` used with `try_into().expect("try into failed")`. If parameters have unexpected padding, the 20-byte extraction could fail.
**Recommendation:** Use proper ABI decoding instead of manual slice operations.

### GREVM-013: `TODO` Comments Indicate Incomplete Implementation

**Files:** `src/hint.rs:267, 285`
**Issue:** Two TODO comments suggest incomplete logic: `TODO(gaoxin): if from_slot == to_slot, what happened?` and the contract type detection TODO.
**Recommendation:** Resolve or document these known limitations.

### GREVM-014: Relaxed Ordering Used for Cross-Thread Coordination

**File:** `src/utils.rs:22-36`
**Issue:** `check_continuous()` uses `Ordering::Relaxed` for both load and CAS operations. While the CAS provides atomicity, Relaxed ordering provides no happens-before guarantee with the flag writes in `add()`.
**Recommendation:** Use `Ordering::Acquire` for loads and `Ordering::Release` for stores to establish proper synchronization.

### GREVM-015: `block_size` vs `txs.len()` Redundancy

**File:** `src/scheduler.rs:262, 307`
**Issue:** `block_size` is always set to `txs.len()` but both are stored separately. If they ever diverge, index-out-of-bounds panics will occur.
**Recommendation:** Remove `block_size` and use `self.txs.len()` directly.

### GREVM-016: No Maximum Transaction Count Limit

**File:** `src/scheduler.rs:299-330`
**Issue:** `Scheduler::new()` allocates `Vec` of size `num_txs` for states, results, dependency tracking, etc. No upper bound is enforced.
**Impact:** A block with millions of transactions could cause OOM.
**Recommendation:** Add a `MAX_BLOCK_SIZE` constant and validate at construction.

---

## INFO (3)

### GREVM-INFO-001: Dead `func_id` Initialization
**File:** `src/hint.rs:290`
**Issue:** `let func_id: u32 = 0;` is immediately shadowed by line 296. Dead code.

### GREVM-INFO-002: Metrics Not Gated Behind Feature Flag
**File:** `src/scheduler.rs:41-79`
**Issue:** `ExecuteMetrics` is always initialized even if no metrics backend is configured.

### GREVM-INFO-003: `clone()` on `Address` and `U256` (Copy types)
**File:** `src/storage.rs:206, 335, 382, 417`
**Issue:** `.clone()` called on `Copy` types. Harmless but noisy.
