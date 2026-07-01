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
//! - [`redelegate_preserves_existing_storage`]  — re-delegating an EOA that already has storage
//!   must preserve it (the block-22546209 `new_ca` storage bug)
//! - [`reset_then_redelegate_preserves_existing_storage`] — same, across a reset-to-EOA step
//! - [`selfdestruct_then_recreate_clears_storage`] — self-destruct + CREATE re-create *clears*
//!   storage (Shanghai; the original `new_ca` purpose — the create side of the same coin)
//! - [`create2_target_redelegate_and_selfdestruct`] — CREATE2 + EIP-7702 re-delegation +
//!   self-destruct composed in one Prague block

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
    Address, B256, HashMap, KECCAK_EMPTY, TxKind, U256, alloy_primitives::U160, hardfork::SpecId,
    keccak256,
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
            ..Default::default()
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
            ..Default::default()
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
    run_with_db(build_db(), txs);
}

fn run_with_db(db: InMemoryDB, txs: Vec<TxEnv>) {
    // Prague enables EIP-7702. `disable_nonce_check = true` only relaxes the tx-sender nonce
    // (every sponsor/caller is distinct here anyway); the authorization-nonce check, which
    // orders the delegations on a shared authority, is independent and still enforced.
    execute::compare_evm_execute_with_spec(
        db,
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

/// Runtime that copies the executing account's storage slot 0 into slot 1:
/// `PUSH1 0x00; SLOAD; PUSH1 0x01; SSTORE; STOP`. Used as a delegate target so that a
/// delegated EOA's *pre-existing* slot-0 value becomes observable (as slot 1) after a CALL.
fn copy_slot0_to_slot1_code() -> Bytecode {
    Bytecode::new_raw(vec![0x60, 0x00, 0x54, 0x60, 0x01, 0x55, 0x00].into())
}

/// Base DB where authority `A` is **already delegated** (to X, from an earlier block) and
/// carries storage `slot0 = stored`. Used to exercise the re-delegation storage-preservation
/// path. `A`'s nonce starts at `nonce`.
fn db_with_predelegated_a(nonce: u64, stored: u64) -> InMemoryDB {
    let mut accounts = account::mock_block_accounts(BLOCK_SIZE);

    let base_designator = Bytecode::new_eip7702(target_x());
    let a = PlainAccount {
        info: AccountInfo {
            balance: U256::from(ONE_ETHER),
            nonce,
            code_hash: base_designator.hash_slow(),
            code: Some(base_designator.clone()),
            ..Default::default()
        },
        storage: [(U256::from(0), U256::from(stored))].into_iter().collect(),
    };
    accounts.insert(authority_a(), a);

    // The re-delegation target copies slot0 -> slot1, making the preserved value observable.
    let reader = copy_slot0_to_slot1_code();
    accounts.insert(target_y(), contract_account(&reader));

    let mut bytecodes = HashMap::default();
    bytecodes.insert(base_designator.hash_slow(), base_designator);
    bytecodes.insert(reader.hash_slow(), reader);
    InMemoryDB::new(accounts, bytecodes, Default::default())
}

/// Regression for the EIP-7702 **storage-preservation** bug (mainnet block 22546209): an EOA
/// that is *already* delegated and carries storage is re-delegated within the block, then
/// CALLed. Its pre-existing storage must survive the in-block code change.
///
/// `A` (slot0 = 42) is re-delegated X -> Y (tx 10); the CALL (tx 30) runs Y, which copies
/// slot0 into slot1, so slot1 must end up 42. Before the fix, grevm's `new_ca` shortcut
/// mistook the in-block code change for a freshly created contract and returned 0 for the
/// unwritten slot0, so slot1 became 0 — diverging from the sequential reference.
#[test]
fn redelegate_preserves_existing_storage() {
    let mut txs: Vec<TxEnv> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegate_tx(10, &[(authority_a(), target_y(), 5)]); // re-delegate A: X -> Y (nonce 5 -> 6)
    txs[30] = call_tx(30, authority_a()); // runs Y: slot1 := slot0 (must be 42, not 0)
    run_with_db(db_with_predelegated_a(5, 42), txs);
}

/// Same storage-preservation guarantee across a reset: `A` (slot0 = 42, already delegated) is
/// reset to a plain EOA (tx 10) and then delegated to Y (tx 20). Neither a reset nor a
/// (re)delegation clears storage, so the CALL (tx 30) running Y must still observe slot0 == 42.
#[test]
fn reset_then_redelegate_preserves_existing_storage() {
    let mut txs: Vec<TxEnv> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegate_tx(10, &[(authority_a(), Address::ZERO, 5)]); // A -> EOA  (nonce 5 -> 6)
    txs[20] = delegate_tx(20, &[(authority_a(), target_y(), 6)]); // A -> Y    (nonce 6 -> 7)
    txs[30] = call_tx(30, authority_a()); // runs Y: slot1 := slot0 (must be 42)
    run_with_db(db_with_predelegated_a(5, 42), txs);
}

/// A CREATE transaction whose calldata is `init_code`.
fn create_tx(caller_idx: usize, init_code: Vec<u8>) -> TxEnv {
    TxEnv {
        caller: account::mock_eoa_address(caller_idx),
        kind: TxKind::Create,
        data: init_code.into(),
        value: U256::ZERO,
        gas_limit: 300_000,
        gas_price: 1,
        nonce: 1,
        ..TxEnv::default()
    }
}

/// The counterpart of [`redelegate_preserves_existing_storage`]: the *other* in-block way an
/// account's `code_hash` changes is a genuine (re)creation, which **does** clear storage. This is
/// the scenario the `new_ca` shortcut was originally added for; the fix must keep it working, since
/// `storage_cleared_in_block` still returns `true` for created accounts (`is_created == true`).
///
/// Under a pre-Cancun spec (Shanghai), `SELFDESTRUCT` fully deletes a pre-existing contract — its
/// storage included — so a `CREATE` landing on the same address produces a fresh contract whose
/// old storage is gone. Here a victim contract (slot0 = 99) self-destructs (tx 10); a CREATE then
/// redeploys a "copy slot0 -> slot1" runtime at the very same address (tx 50); a later CALL (tx
/// 80) must observe the *cleared* slot0 (slot1 == 0), not the stale base-DB 99. Asserts grevm's
/// parallel result matches sequential for the full self-destruct → recreate → read lifecycle.
#[test]
fn selfdestruct_then_recreate_clears_storage() {
    let creator_idx = 50;
    let creator = account::mock_eoa_address(creator_idx);
    // Address the CREATE in tx `creator_idx` (creator nonce 1) will deploy to.
    let victim = creator.create(1);

    let selfdestruct_code = Bytecode::new_raw(vec![0x33, 0xff].into()); // CALLER; SELFDESTRUCT
    let runtime = vec![0x60, 0x00, 0x54, 0x60, 0x01, 0x55, 0x00]; // copy slot0 -> slot1
    let recreated = Bytecode::new_raw(runtime.clone().into());
    // Init code: CODECOPY the 7-byte runtime (at offset 12) to memory and RETURN it.
    let mut init_code =
        vec![0x60, 0x07, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, 0x07, 0x60, 0x00, 0xf3];
    init_code.extend_from_slice(&runtime);

    let mut accounts = account::mock_block_accounts(BLOCK_SIZE);
    let v = PlainAccount {
        info: AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash: selfdestruct_code.hash_slow(),
            code: Some(selfdestruct_code.clone()),
            ..Default::default()
        },
        storage: [(U256::from(0), U256::from(99))].into_iter().collect(),
    };
    accounts.insert(victim, v);

    let mut bytecodes = HashMap::default();
    bytecodes.insert(selfdestruct_code.hash_slow(), selfdestruct_code);
    bytecodes.insert(recreated.hash_slow(), recreated);
    let db = InMemoryDB::new(accounts, bytecodes, Default::default());

    let mut txs: Vec<TxEnv> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = call_tx(10, victim); // self-destruct the victim (Shanghai => deleted)
    txs[creator_idx] = create_tx(creator_idx, init_code); // redeploy "copy slot0->slot1" at victim
    txs[80] = call_tx(80, victim); // runs recreated: slot1 := slot0, which must be 0 (cleared)

    execute::compare_evm_execute_with_spec(
        db,
        txs,
        false,
        true,
        Default::default(),
        SpecId::SHANGHAI,
    );
}

/// Init code that, when run, deploys `runtime` as the new account's code
/// (`CODECOPY` the trailing runtime to memory and `RETURN` it). Assumes `runtime.len() < 256`.
fn deploy_initcode(runtime: &[u8]) -> Vec<u8> {
    let l = runtime.len() as u8;
    let mut c = vec![0x60, l, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, l, 0x60, 0x00, 0xf3];
    c.extend_from_slice(runtime);
    c
}

/// A factory contract whose body `CREATE2`s `init_code` with salt 0 when called.
fn create2_factory(init_code: &[u8]) -> Bytecode {
    let m = init_code.len() as u8;
    let mut c = vec![
        0x60, m, 0x60, 0x11, 0x60, 0x00, 0x39, // CODECOPY init_code -> mem[0]
        0x60, 0x00, 0x60, m, 0x60, 0x00, 0x60, 0x00,
        0xf5, // CREATE2(value=0,off=0,size=m,salt=0)
        0x00, // STOP
    ];
    c.extend_from_slice(init_code);
    Bytecode::new_raw(c.into())
}

/// All three mechanisms in one Prague block: a fresh delegate target is **CREATE2**-deployed, a
/// stored EOA is **EIP-7702** re-delegated to it, and a contract **self-destructs** — verifying
/// they compose under grevm's parallel scheduler.
///
/// NB: a *single* block where SELFDESTRUCT actually **clears** storage AND EIP-7702 is active is
/// impossible — EIP-6780 (Cancun) restricts self-destruct clearing to same-tx-created accounts, so
/// cross-tx "self-destruct → CREATE2 recreate" only clears pre-Cancun, while EIP-7702 is Prague.
/// (The clearing half is covered by [`selfdestruct_then_recreate_clears_storage`] under Shanghai.)
/// Here, under Prague, the self-destruct of a pre-existing `V` is balance-only.
///
/// Flow: tx 10 CREATE2-deploys `T` ("copy slot0 -> slot1", `is_created == true`); tx 20
/// re-delegates the stored EOA `A` (slot0 = 42) to `T` (`is_created == false` — storage must be
/// preserved); tx 30 self-destructs `V`, forwarding its balance to `A`; tx 40 CALLs `A`, running
/// `T`, which must read `A`'s preserved slot0 (so slot1 == 42, not 0). `A`'s slot0 read flows
/// through grevm's `storage_cleared_in_block` gate, so this also fails under the pre-fix `new_ca`
/// logic.
#[test]
fn create2_target_redelegate_and_selfdestruct() {
    let factory = Address::from(U160::from(920_000));
    let victim = Address::from(U160::from(920_001));

    let runtime = vec![0x60, 0x00, 0x54, 0x60, 0x01, 0x55, 0x00]; // copy slot0 -> slot1
    let init_code = deploy_initcode(&runtime);
    let target = factory.create2(B256::ZERO, keccak256(&init_code)); // where CREATE2 deploys T

    let factory_code = create2_factory(&init_code);
    let base_designator = Bytecode::new_eip7702(target_x()); // A starts delegated elsewhere
    let mut selfdestruct = vec![0x73]; // PUSH20 A; SELFDESTRUCT (forward balance to A)
    selfdestruct.extend_from_slice(authority_a().as_slice());
    selfdestruct.push(0xff);
    let victim_code = Bytecode::new_raw(selfdestruct.into());

    let mut accounts = account::mock_block_accounts(BLOCK_SIZE);
    // A: already delegated (to X) and carrying storage slot0 = 42.
    accounts.insert(
        authority_a(),
        PlainAccount {
            info: AccountInfo {
                balance: U256::from(ONE_ETHER),
                nonce: 5,
                code_hash: base_designator.hash_slow(),
                code: Some(base_designator.clone()),
                ..Default::default()
            },
            storage: [(U256::from(0), U256::from(42))].into_iter().collect(),
        },
    );
    accounts.insert(factory, contract_account(&factory_code));
    let mut victim_account = contract_account(&victim_code);
    victim_account.info.balance = U256::from(500u128);
    accounts.insert(victim, victim_account);

    let mut bytecodes = HashMap::default();
    for c in [&base_designator, &factory_code, &victim_code] {
        bytecodes.insert(c.hash_slow(), c.clone());
    }
    let db = InMemoryDB::new(accounts, bytecodes, Default::default());

    let mut txs: Vec<TxEnv> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = call_tx(10, factory); // CREATE2-deploy T
    txs[20] = delegate_tx(20, &[(authority_a(), target, 5)]); // re-delegate A -> T (nonce 5 -> 6)
    txs[30] = call_tx(30, victim); // V self-destructs, balance -> A
    txs[40] = call_tx(40, authority_a()); // run T: slot1 := slot0 (must be 42, preserved)
    run_with_db(db, txs);
}
