#![allow(missing_docs)]

//! End-to-end EIP-7702 regression tests for Grevm's parallel executor.
//!
//! Every test builds a full block, runs it through both the parallel scheduler and a
//! sequential revm reference, and asserts the two agree on per-tx execution results
//! *and* bundle state via [`execute::compare_evm_execute_with_spec`] (Prague spec).
//!
//! The block is padded to at least `MIN_PARALLEL_TXS` (64) independent transactions so the
//! scheduler stays on the parallel path instead of falling back to sequential — the
//! multi-version-memory code-publication bug under test only manifests across the shared
//! memory of parallel transactions.
//!
//! Coverage:
//! - [`single_delegation_then_call`]            — positive control (EOA → contract → CALL)
//! - [`redelegate_within_block_uses_latest_code`] — A → X then A → Y in one block (the fix)
//! - [`reset_to_eoa_then_call_is_noop`]         — A → X then A → 0x0 (delegation cleared)
//! - [`reset_then_redelegate_within_block`]     — A → X → 0x0 → Y chained in one block
//! - [`multiple_authorities_single_tx`]         — one tx delegating two distinct authorities
//! - [`repeated_authority_single_tx`]           — one tx with two tuples for the same authority

use grevm::test_utils::{
    TRANSFER_GAS_LIMIT,
    common::{account, execute, storage::InMemoryDB},
};
use revm_context::{
    TxEnv,
    either::Either,
    transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
};
use revm_database::PlainAccount;
use revm_primitives::{
    Address, HashMap, KECCAK_EMPTY, TxKind, U256, alloy_primitives::U160, hardfork::SpecId,
};
use revm_state::{AccountInfo, Bytecode};

/// Keep the block comfortably above `MIN_PARALLEL_TXS` so grevm runs in parallel.
const BLOCK_SIZE: usize = 100;
const EIP7702_TX_TYPE: u8 = 4;
const ONE_ETHER: u128 = 1_000_000_000_000_000_000;

// Addresses for the EIP-7702 actors, placed well outside the mock-EOA range
// (`mock_eoa_address` lives at `idx + 1000`).
fn authority_a() -> Address {
    Address::from(U160::from(900_000))
}
fn authority_b() -> Address {
    Address::from(U160::from(900_001))
}
fn target_x() -> Address {
    Address::from(U160::from(910_000))
}
fn target_y() -> Address {
    Address::from(U160::from(910_001))
}

/// Runtime code `PUSH1 <value>; PUSH1 0x00; SSTORE; STOP`: stores `value` into slot 0 of
/// whatever account is currently executing. When an EOA delegates here (EIP-7702) and is
/// then CALLed, the SSTORE lands in the *EOA's* storage — making "which target did the
/// delegation resolve to" observable as the EOA's slot-0 value.
fn sstore_code(value: u8) -> Bytecode {
    Bytecode::new_raw(vec![0x60u8, value, 0x60, 0x00, 0x55, 0x00].into())
}

fn contract_account(code: &Bytecode) -> PlainAccount {
    PlainAccount {
        info: AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash: code.hash_slow(),
            code: Some(code.clone()),
        },
        storage: Default::default(),
    }
}

fn eoa_account(balance: u128, nonce: u64) -> PlainAccount {
    PlainAccount {
        info: AccountInfo {
            balance: U256::from(balance),
            nonce,
            code_hash: KECCAK_EMPTY,
            code: None,
        },
        storage: Default::default(),
    }
}

/// Build the base DB: `BLOCK_SIZE` mock EOAs + miner, two fresh authority EOAs (`A`, `B`,
/// both nonce 0) and two delegate targets `X` (stores 1) and `Y` (stores 2). Authorities
/// not touched by a given test simply stay untouched and never enter the bundle.
fn build_db() -> InMemoryDB {
    let mut accounts = account::mock_block_accounts(BLOCK_SIZE);
    accounts.insert(authority_a(), eoa_account(ONE_ETHER, 0));
    accounts.insert(authority_b(), eoa_account(ONE_ETHER, 0));

    let cx = sstore_code(1);
    let cy = sstore_code(2);
    accounts.insert(target_x(), contract_account(&cx));
    accounts.insert(target_y(), contract_account(&cy));

    let mut bytecodes = HashMap::default();
    bytecodes.insert(cx.hash_slow(), cx);
    bytecodes.insert(cy.hash_slow(), cy);

    InMemoryDB::new(accounts, bytecodes, Default::default())
}

/// Independent self-transfer used as parallel-friendly padding.
fn padding_tx(idx: usize) -> TxEnv {
    let addr = account::mock_eoa_address(idx);
    TxEnv {
        caller: addr,
        kind: TxKind::Call(addr),
        value: U256::from(1),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce: 1,
        ..TxEnv::default()
    }
}

/// A type-4 tx (sponsored by `mock_eoa(sponsor_idx)`) carrying one or more authorization
/// tuples `(authority, target, authority_nonce)`. `authority_nonce` must equal the
/// authority account's nonce at the moment the tuple is applied (it bumps by one per
/// applied tuple), otherwise revm silently skips it. A `target` of `Address::ZERO` clears
/// the delegation, turning the authority back into a plain EOA.
fn delegate_tx(sponsor_idx: usize, auths: &[(Address, Address, u64)]) -> TxEnv {
    let authorization_list = auths
        .iter()
        .map(|&(authority, target, nonce)| {
            let auth = Authorization { chain_id: U256::ZERO, address: target, nonce };
            Either::Right(RecoveredAuthorization::new_unchecked(
                auth,
                RecoveredAuthority::Valid(authority),
            ))
        })
        .collect();
    let sponsor = account::mock_eoa_address(sponsor_idx);
    TxEnv {
        tx_type: EIP7702_TX_TYPE,
        caller: sponsor,
        // call the sponsor itself (an EOA, no code) → no-op; the delegation is applied in
        // pre-execution regardless of the call target.
        kind: TxKind::Call(sponsor),
        value: U256::ZERO,
        gas_limit: 400_000,
        gas_price: 1,
        nonce: 1,
        authorization_list,
        ..TxEnv::default()
    }
}

fn call_tx(caller_idx: usize, to: Address) -> TxEnv {
    TxEnv {
        caller: account::mock_eoa_address(caller_idx),
        kind: TxKind::Call(to),
        value: U256::ZERO,
        gas_limit: 100_000,
        gas_price: 1,
        nonce: 1,
        ..TxEnv::default()
    }
}

fn run(txs: Vec<TxEnv>) {
    // Prague enables EIP-7702. `disable_nonce_check = true` only relaxes the tx-sender nonce
    // (every sponsor/caller is distinct here anyway); the authorization-nonce check, which
    // orders the delegations on a shared authority, is independent and still enforced.
    execute::compare_evm_execute_with_spec(
        build_db(),
        txs,
        false,
        true,
        Default::default(),
        SpecId::PRAGUE,
    );
}

/// Positive control: a single delegation followed by a CALL. Exercises the whole 7702
/// plumbing end-to-end and must agree with the sequential reference both before and after
/// the storage.rs fix.
#[test]
fn single_delegation_then_call() {
    let mut txs: Vec<TxEnv> = (0..BLOCK_SIZE).map(padding_tx).collect();
    // delegate A -> X (A nonce 0 -> 1)
    txs[10] = delegate_tx(10, &[(authority_a(), target_x(), 0)]);
    // CALL A → runs X → A.slot0 == 1
    txs[30] = call_tx(30, authority_a());
    run(txs);
}

/// Regression test for the `new_contract` bug in `update_mv_memory`: within one block A is
/// delegated to X (tx 10) and then re-delegated to Y (tx 20). A later CALL (tx 30) must
/// resolve to Y (slot0 == 2). Before the fix, tx 20's new `Code` entry was dropped from
/// multi-version memory (its read code_hash was already non-empty), so tx 30 resolved the
/// stale X designator and wrote slot0 == 1 — diverging from the sequential reference.
#[test]
fn redelegate_within_block_uses_latest_code() {
    let mut txs: Vec<TxEnv> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegate_tx(10, &[(authority_a(), target_x(), 0)]); // A -> X (nonce 0 -> 1)
    txs[20] = delegate_tx(20, &[(authority_a(), target_y(), 1)]); // A -> Y (nonce 1 -> 2)
    txs[30] = call_tx(30, authority_a()); // must run Y (slot0 == 2), not stale X
    run(txs);
}

/// Delegate A -> X, then clear the delegation with a `0x0` target (tx 20). A is a plain EOA
/// again, so the final CALL (tx 30) is a no-op and A's storage stays empty. Guards the
/// reset path: even though the stale X `Code` entry lingers in multi-version memory, the
/// reader must gate on A's now-empty `code_hash` and never resolve it.
#[test]
fn reset_to_eoa_then_call_is_noop() {
    let mut txs: Vec<TxEnv> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegate_tx(10, &[(authority_a(), target_x(), 0)]); // A -> X (nonce 0 -> 1)
    txs[20] = delegate_tx(20, &[(authority_a(), Address::ZERO, 1)]); // A -> EOA (nonce 1 -> 2)
    txs[30] = call_tx(30, authority_a()); // no code → no-op, A.slot0 stays unset
    run(txs);
}

/// Full lifecycle on a single account in one block: A -> X (tx 10) -> reset (tx 15) -> Y
/// (tx 20), then CALL (tx 30) must resolve to Y (slot0 == 2). Exercises non-empty → empty →
/// non-empty `code_hash` transitions back-to-back.
#[test]
fn reset_then_redelegate_within_block() {
    let mut txs: Vec<TxEnv> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegate_tx(10, &[(authority_a(), target_x(), 0)]); // A -> X   (nonce 0 -> 1)
    txs[15] = delegate_tx(15, &[(authority_a(), Address::ZERO, 1)]); // A -> EOA (nonce 1 -> 2)
    txs[20] = delegate_tx(20, &[(authority_a(), target_y(), 2)]); // A -> Y   (nonce 2 -> 3)
    txs[30] = call_tx(30, authority_a()); // must run Y → slot0 == 2
    run(txs);
}

/// One EIP-7702 tx carrying two tuples for two *distinct* authorities: A -> X and B -> Y
/// (tx 10). Both authorities' `Code` entries must be published so the later CALLs resolve
/// independently (A.slot0 == 1, B.slot0 == 2).
#[test]
fn multiple_authorities_single_tx() {
    let mut txs: Vec<TxEnv> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegate_tx(10, &[(authority_a(), target_x(), 0), (authority_b(), target_y(), 0)]);
    txs[20] = call_tx(20, authority_a()); // runs X → A.slot0 == 1
    txs[30] = call_tx(30, authority_b()); // runs Y → B.slot0 == 2
    run(txs);
}

/// One EIP-7702 tx carrying two tuples for the *same* authority: A -> X then A -> Y in a
/// single tx (nonces 0 and 1). Last write wins, so A ends delegated to Y and its nonce
/// bumps twice; the later CALL must resolve to Y (slot0 == 2).
#[test]
fn repeated_authority_single_tx() {
    let mut txs: Vec<TxEnv> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegate_tx(10, &[(authority_a(), target_x(), 0), (authority_a(), target_y(), 1)]);
    txs[30] = call_tx(30, authority_a()); // must run Y → slot0 == 2
    run(txs);
}
