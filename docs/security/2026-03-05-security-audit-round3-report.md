# Security Audit Report — grevm (Round 3)

**Date:** 2026-03-05
**Scope:** All Rust source in `src/` (~3000 LOC)
**Language:** Rust | **Framework:** revm v29.0.1-gravity fork, Block-STM
**Repository:** `Galxe/grevm` (branch: `security-audit-fixes`)
**Prior Audits:** [Round 1 — 2026-02-26](./2026-02-26-security-audit-report.md) (19 findings), [Round 2 — 2026-03-02](./2026-03-02-security-audit-round2-report.md) (10 findings)

---

## Summary

| Severity | Count |
|----------|-------|
| HIGH | 2 |
| MEDIUM | 4 |
| LOW | 3 |
| INFO | 2 |
| **Total** | **11** |

> [!NOTE]
> This Round 3 audit was performed against the codebase **after** Round 1 and Round 2 fixes were applied.
> It includes verification of prior fix completeness and a cross-module review with gravity-reth, gravity-sdk, and gravity-aptos.

---

## HIGH Severity (2)

### GREVM-R3-001: TOCTOU in `async_finality` — Lock-Drop-Reacquire Still Exploitable

**File:** `grevm/src/scheduler.rs:355-374`

The `async_finality` loop acquires the lock at line 358 to check `status == Unconfirmed`, then **drops the lock** (temporary `MutexGuard`). It performs timestamp checks (lines 361-371) without holding any lock, then **re-acquires** at line 372 and unconditionally sets `Finality`.

The Round 1 reviewer (GREVM-004) stated logical timestamps prevent exploitation. However, the `lower_ts` mechanism has a gap: `reset_validation_idx(txid+1)` sets `lower_ts[txid+1]`, NOT `lower_ts[txid]`. So for `finality_idx = txid`, the timestamp guard does not fire for conflicts detected AT that txid.

**Race sequence:**
1. Finality thread: reads `Unconfirmed` at line 358, drops lock
2. Worker: validates tx, finds conflict, sets `Conflict`, calls `reset_validation_idx(txid+1)` — updates `lower_ts[txid+1]`
3. Finality thread: checks `lower_ts[txid]` (unchanged!), passes, re-acquires lock, overwrites to `Finality`

**Impact:** Transaction committed as final with stale execution results — consensus-breaking state corruption.

**Fix:** Re-check status after re-acquiring the lock:
```rust
let mut tx_state = self.tx_states[finality_idx].lock();
if tx_state.status != TransactionStatus::Unconfirmed {
    break;
}
tx_state.status = TransactionStatus::Finality;
```

**Prior:** GREVM-004 (Round 1). **Still exploitable.**
**Review Comments** reviewer: Xin GAO; state: accepted; comments: Re-check status after re-acquiring the lock

---

### GREVM-R3-002: CommitGuard Does Not Prevent Simultaneous Mutable Aliases

**File:** `grevm/src/async_commit.rs:13-27`, `scheduler.rs:456,580`

`CommitGuard::new()` takes `&UnsafeCell<ParallelState<DB>>` (shared ref). Two `CommitGuard` instances can coexist — one at line 456 (`parallel_execute`) and one at line 580 (`fallback_sequential`). While the current code ensures temporal separation (scope joins before fallback), the type system provides no enforcement.

Additionally, `state_ref()` (line 68-71) creates `&ParallelState` while `state_mut()` creates `&mut ParallelState` from the same `UnsafeCell`, within the same `commit()` method — technically UB under Rust's aliasing rules even though the references don't overlap temporally within the method.

**Impact:** The CommitGuard pattern fails to provide the compile-time safety GREVM-R2-001 was designed to deliver. Future refactors could silently introduce UB.

**Prior:** GREVM-R2-001 (Round 2). **Incomplete fix.**
**Review Comments** reviewer: Xin GAO; state: rejected; comments: The fix is overly complex; the two functions are temporally exclusive by design—a defining characteristic of the Block-STM algorithm.

---

## MEDIUM Severity (4)

### GREVM-R3-003: `nonce + 1` Overflow in Nonce Assertion

**File:** `grevm/src/async_commit.rs:105`

`assert_eq!(change.info.nonce, expect + 1)` — if `expect` is `u64::MAX`, `expect + 1` wraps to 0 in release mode (overflow-checks disabled). The assertion would compare against 0 instead of the intended value.
**Review Comments** reviewer: Xin GAO; state: rejected; comments: If u64::MAX is reached, the system will face more critical issues. This is a lower-priority edge case.

### GREVM-R3-004: Write Set Not Cleaned on EVM Error Re-execution

**File:** `grevm/src/scheduler.rs:719-739`

When a tx encounters an EVM error, the old write_set from a previous successful incarnation is preserved and stored back. The stale `mv_memory` entries remain. During re-execution, `write_new_locations` flag may be incorrectly computed, potentially skipping necessary revalidation of dependent transactions.
**Review Comments** reviewer: Xin GAO; state: rejected; comments: This is a characteristic behavior of the Block-STM algorithm; stale entries are handled by validation re-execution on dependent transactions.

### GREVM-R3-005: `key_tx` Dependency Race with Concurrent `commit`

**File:** `grevm/src/tx_dependency.rs:112-123`

Race between `key_tx(txid)` and `commit(txid-1)` can cause unnecessary double-scheduling. Benign in current code but creates latent performance degradation under pathological workloads.
**Review Comments** reviewer: Xin GAO; state: rejected; comments: This scenario cannot occur; commit signifies a transaction is finalized and will not be re-executed. Once committed, transaction state is immutable.

### GREVM-R3-006: `update_mv_memory` Coinbase Skip Hides User ETH Transfers

**File:** `grevm/src/storage.rs:208-210`

All coinbase changes are skipped in `update_mv_memory`, including **user-initiated ETH transfers** to the coinbase address. If tx A sends ETH to coinbase and tx B reads the coinbase balance, the change is invisible in MVMemory. The `accurate_origin` flag partially mitigates this but has a gap: when `commit_idx == current_tx.txid` and a preceding executed-but-uncommitted tx sent ETH to coinbase.

**Impact:** Potential stale coinbase balance reads causing incorrect execution that validation may not catch.
**Review Comments** reviewer: Xin GAO; state: rejected; comments: The condition `commit_idx == current_tx.txid` ensures the transaction reads the correct miner reward directly from the committed state, preventing stale reads.

---

## LOW Severity (3)

### GREVM-R3-007: ERC20 Transfer Uses Slot 0, TransferFrom Uses Slot 1 for Balances

**File:** `grevm/src/hint.rs:236-253 vs 255-280`

Inconsistent storage slot assumptions between Transfer (slot 0) and TransferFrom (slot 1) for balance mappings. Performance-only impact since validation catches all conflicts.
**Review Comments** reviewer: Xin GAO; state: rejected; comments: Hints are still experimental in nature. Even if incorrect, they do not affect the correctness of final execution due to OCC validation.

### GREVM-R3-008: `results.push(result)` Before Error Check

**File:** `grevm/src/async_commit.rs:136-139`

The execution result is pushed to `self.results` even when the nonce check fails. The results vector contains an entry for a transaction whose state was never committed.

**Prior:** GREVM-R2-002. **Partially fixed.**
**Review Comments** reviewer: Xin GAO; state: rejected; comments: The caller's error handling returns immediately on nonce mismatch, preventing further use of invalid results. However, reordering the push after validation would improve code clarity.

### GREVM-R3-009: `lazy_reward` Applied After Error Not Guarded at Method Entry

**File:** `grevm/src/async_commit.rs:141-151`

The `commit()` method does not check `self.commit_result` at entry. If a caller fails to check between calls, state corruption could compound. Mitigated by caller's check in `async_commit()`.
**Review Comments** reviewer: Xin GAO; state: rejected; comments: The caller (`async_commit()`) enforces error checking before any subsequent operation. Adding a guard clause at method entry would improve defensive programming despite caller protections.

---

## INFO (2)

### GREVM-R3-010: Block-STM Validation Invariant Confirmed ✓

The Block-STM validation loop correctly detects all read-write conflicts and triggers re-execution. The OCC-based approach ensures eventual consistency even when hints are incorrect.
**Review Comments** reviewer: Xin GAO; state: accepted; comments: Validation correctly maintains OCC invariants; all dependencies are enforced through re-execution.

### GREVM-R3-011: DAG-based Scheduling Correctly Parallelizes Independent Transactions ✓

The hint-based DAG construction and partition assignment produce correct dependency ordering. Mis-hints only affect performance, not correctness, due to the validation fallback.
**Review Comments** reviewer: Xin GAO; state: accepted; comments: DAG scheduling correctly enforces transaction dependencies; partition assignment is sound.

---

## Fix Verification from Prior Rounds

| Prior Finding | Current Status |
|---------------|----------------|
| GREVM-004 (TOCTOU in async_finality) | **Still exploitable** — see GREVM-R3-001 |
| GREVM-006 (CAS loop in next_validation_idx) | **Not implemented** — fix checklist incorrectly marks as done |
| GREVM-R2-001 (CommitGuard) | **Incomplete** — see GREVM-R3-002 |
| GREVM-R2-002 (Early return on nonce) | **Partially done** — results.push before error check |
| GREVM-R2-003 (empty partition guard) | **Partially done** — parallel_cnt not capped |
| All other fixes | Correctly applied |

---

## Cumulative Statistics (Rounds 1-3)

| Severity | R1 | R2 | R3 | Total |
|----------|----|----|-----|-------|
| CRITICAL | 1 | 0 | 0 | 1 |
| HIGH | 5 | 2 | 2 | 9 |
| MEDIUM | 5 | 3 | 4 | 12 |
| LOW | 5 | 3 | 3 | 11 |
| INFO | 3 | 2 | 2 | 7 |
| **Total** | **19** | **10** | **11** | **40** |
