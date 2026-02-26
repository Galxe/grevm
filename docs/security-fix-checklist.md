# Security Fix Checklist — grevm

**Audit Date:** 2026-02-26
**Total Findings:** 19 (3 CRITICAL, 3 HIGH, 5 MEDIUM, 5 LOW, 3 INFO)

## CRITICAL

- [ ] **GREVM-001** — Replace all 7 instances of `&T → &mut T` unsafe casting with proper interior mutability
  - [ ] `src/utils.rs:42-44` — `ContinuousDetectSet::add()`: use `Vec<AtomicBool>`
  - [ ] `src/scheduler.rs:551-555` — `state_mut()`: refactor ownership for fallback sequential
  - [ ] `src/async_commit.rs:40-44` — `state_mut()`: separate mutable handle for commit thread
  - [ ] `src/hint.rs:123-124` — `parse_hints()`: partition `rw_set` slices per thread
  - [ ] `src/storage.rs:53-64` — `parallel_apply_transitions_and_create_reverts()`: use `UnsafeCell` or `split_at_mut()`
- [ ] **GREVM-002** — Replace `Vec<bool>` in `ContinuousDetectSet` with `Vec<AtomicBool>`
  - [ ] Verify `check_continuous()` memory ordering is correct with `Acquire/Release`
- [ ] **GREVM-003** — Separate `ParallelState` into read-only and write-only handles
  - [ ] Ensure commit thread has exclusive ownership of `transition_state`
  - [ ] Worker threads access only the immutable database reference

## HIGH

- [ ] **GREVM-004** — Fix TOCTOU in `async_finality`: hold lock across check-and-set
  - [ ] Verify that holding the lock doesn't cause performance regression
- [ ] **GREVM-005** — Fix lock ordering in `TxDependency::add()` and `remove()`
  - [ ] Establish consistent global lock ordering (lower index first)
  - [ ] Add deadlock detection in tests
- [ ] **GREVM-006** — Replace load + fetch_add with CAS loop in `next_validation_idx()`
  - [ ] Add test for concurrent validation index advancement

## MEDIUM

- [ ] **GREVM-007** — Replace `panic!`/`assert!`/`unwrap()` in production paths with error returns
  - [ ] `src/async_commit.rs:78` — nonce assertion
  - [ ] `src/async_commit.rs:120` — balance increment assertion
  - [ ] `src/scheduler.rs:402` — commit error panic
  - [ ] `src/scheduler.rs:530, 618, 757-758, 762, 765` — scheduler panics
- [ ] **GREVM-008** — Use `parse().unwrap_or(default)` for env var parsing
- [ ] **GREVM-009** — Fix `get_contract_type()` to return `UNKNOWN` for non-ERC20 contracts
- [ ] **GREVM-010** — Replace `println!()` with structured logging (`tracing::warn!`)
- [ ] **GREVM-011** — Document safety invariants for `fork_join_util` parallel mutation

## LOW

- [ ] **GREVM-012** — Use proper ABI decoding in hint parameter extraction
- [ ] **GREVM-013** — Resolve TODO comments in `hint.rs`
- [ ] **GREVM-014** — Upgrade memory ordering in `ContinuousDetectSet` to Acquire/Release
- [ ] **GREVM-015** — Remove redundant `block_size` field
- [ ] **GREVM-016** — Add `MAX_BLOCK_SIZE` validation in `Scheduler::new()`

## INFO (No action required)

- [x] **GREVM-INFO-001** — Dead `func_id` initialization (cosmetic)
- [x] **GREVM-INFO-002** — Metrics always initialized (no impact)
- [x] **GREVM-INFO-003** — `.clone()` on Copy types (cosmetic)

---

## Test Plan

- [ ] Run existing test suite: `cargo test`
- [ ] Run benchmarks to verify no performance regression: `cargo bench`
- [ ] Run with Miri (where possible) to check UB: `cargo +nightly miri test`
- [ ] Run with ThreadSanitizer: `RUSTFLAGS="-Z sanitizer=thread" cargo +nightly test`
- [ ] Stress test with high concurrency levels (`GREVM_CONCURRENT_LEVEL=32`)
