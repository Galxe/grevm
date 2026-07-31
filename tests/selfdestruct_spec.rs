#![allow(missing_docs)]

//! Cross-fork SELFDESTRUCT and EIP-161 differential tests.
//!
//! These fixtures deliberately run a full parallel-sized block. Each case first executes through
//! the upstream sequential revm reference, then requires Grevm to produce identical transaction
//! results and bundle state. A few protocol-level assertions on the sequential bundle keep the
//! tests from becoming circular scheduler-only checks.

use grevm::test_utils::common::{account, execute, storage::InMemoryDB};
use revm_context::{BlockEnv, CfgEnv, TxEnv};
use revm_database::{PlainAccount, states::BundleState};
use revm_primitives::{
    Address, B256, Bytes, HashMap, KECCAK_EMPTY, TxKind, U256, alloy_primitives::U160,
    hardfork::SpecId,
};
use revm_state::{AccountInfo, Bytecode};

const BLOCK_SIZE: usize = 100;
const CALL_GAS_LIMIT: u64 = 300_000;
const CREATE_GAS_LIMIT: u64 = 500_000;
const VICTIM_BALANCE: u64 = 777;
const OLD_STORAGE_VALUE: u64 = 42;

fn victim() -> Address {
    Address::from(U160::from(930_000))
}

fn probe() -> Address {
    Address::from(U160::from(930_001))
}

fn parent() -> Address {
    Address::from(U160::from(930_002))
}

fn missing() -> Address {
    Address::from(U160::from(930_003))
}

fn receiver() -> Address {
    account::mock_eoa_address(BLOCK_SIZE - 1)
}

fn plain_account(
    info: AccountInfo,
    storage: impl IntoIterator<Item = (U256, U256)>,
) -> PlainAccount {
    PlainAccount { info, storage: storage.into_iter().collect() }
}

fn contract_account(code: &Bytecode, balance: u64) -> PlainAccount {
    plain_account(
        AccountInfo {
            balance: U256::from(balance),
            nonce: 1,
            code_hash: code.hash_slow(),
            code: Some(code.clone()),
            ..Default::default()
        },
        [],
    )
}

fn insert_contract(
    accounts: &mut HashMap<Address, PlainAccount>,
    bytecodes: &mut HashMap<B256, Bytecode>,
    address: Address,
    code: Bytecode,
    balance: u64,
) {
    accounts.insert(address, contract_account(&code, balance));
    bytecodes.insert(code.hash_slow(), code);
}

fn selfdestruct_to(target: Address) -> Bytecode {
    let mut code = vec![0x73]; // PUSH20 target
    code.extend_from_slice(target.as_slice());
    code.push(0xff); // SELFDESTRUCT
    Bytecode::new_raw(code.into())
}

/// `BALANCE observed; PUSH1 0; SSTORE; STOP`.
///
/// BALANCE loads the observed account without touching it, which exercises a later Basic read after
/// a deletion marker without materializing the deleted account under pre-EIP-161 rules.
fn balance_probe(observed: Address) -> Bytecode {
    let mut code = vec![0x73]; // PUSH20 observed
    code.extend_from_slice(observed.as_slice());
    code.extend_from_slice(&[0x31, 0x60, 0x00, 0x55, 0x00]);
    Bytecode::new_raw(code.into())
}

/// Call `victim` with zero value, discard the result, then revert the enclosing frame.
fn call_then_revert(victim: Address) -> Bytecode {
    let mut code = Vec::new();
    for _ in 0..5 {
        code.extend_from_slice(&[0x60, 0x00]); // out/in sizes and offsets, value
    }
    code.push(0x73); // PUSH20 victim
    code.extend_from_slice(victim.as_slice());
    code.extend_from_slice(&[
        0x61, 0xff, 0xff, // PUSH2 gas
        0xf1, // CALL
        0x50, // POP success
        0x60, 0x00, 0x60, 0x00, 0xfd, // REVERT(0, 0)
    ]);
    Bytecode::new_raw(code.into())
}

/// Runtime: `slot0 += 1; SELFDESTRUCT(receiver)`.
fn increment_then_selfdestruct(receiver: Address) -> Vec<u8> {
    let mut code = vec![
        0x60, 0x00, // PUSH1 0
        0x54, // SLOAD
        0x60, 0x01, // PUSH1 1
        0x01, // ADD
        0x60, 0x00, // PUSH1 0
        0x55, // SSTORE
        0x73, // PUSH20 receiver
    ];
    code.extend_from_slice(receiver.as_slice());
    code.push(0xff);
    code
}

fn deploy_initcode(runtime: &[u8]) -> Vec<u8> {
    assert!(runtime.len() < 256);
    let len = runtime.len() as u8;
    let mut init = vec![
        0x60, len, 0x60, 0x0c, 0x60, 0x00, 0x39, // CODECOPY
        0x60, len, 0x60, 0x00, 0xf3, // RETURN
    ];
    init.extend_from_slice(runtime);
    init
}

fn padding_tx(index: usize) -> TxEnv {
    let caller = account::mock_eoa_address(index);
    TxEnv {
        caller,
        kind: TxKind::Call(caller),
        gas_limit: 21_000,
        gas_price: 1,
        nonce: 1,
        ..TxEnv::default()
    }
}

fn call_tx(index: usize, to: Address) -> TxEnv {
    TxEnv {
        caller: account::mock_eoa_address(index),
        kind: TxKind::Call(to),
        gas_limit: CALL_GAS_LIMIT,
        gas_price: 1,
        nonce: 1,
        ..TxEnv::default()
    }
}

fn create_tx(index: usize, initcode: Vec<u8>) -> TxEnv {
    TxEnv {
        caller: account::mock_eoa_address(index),
        kind: TxKind::Create,
        data: Bytes::from(initcode),
        gas_limit: CREATE_GAS_LIMIT,
        gas_price: 1,
        nonce: 1,
        ..TxEnv::default()
    }
}

fn block_txs() -> Vec<TxEnv> {
    (0..BLOCK_SIZE).map(padding_tx).collect()
}

fn run_differential(
    spec: SpecId,
    db: InMemoryDB,
    txs: Vec<TxEnv>,
    beneficiary: Address,
) -> BundleState {
    let mut cfg = CfgEnv::new_with_spec(spec);
    cfg.disable_nonce_check = true;
    let env = BlockEnv { beneficiary, ..BlockEnv::default() };
    let (_, bundle) = execute::execute_revm_sequential(db.clone(), cfg.clone(), env.clone(), &txs)
        .unwrap_or_else(|error| panic!("{spec:?} sequential execution failed: {error:?}"));
    execute::compare_evm_execute_with_env(db, txs, cfg, env, Default::default());
    bundle
}

fn all_specs() -> [SpecId; 6] {
    [
        SpecId::FRONTIER,
        SpecId::SPURIOUS_DRAGON,
        SpecId::SHANGHAI,
        SpecId::CANCUN,
        SpecId::PRAGUE,
        SpecId::AMSTERDAM,
    ]
}

#[test]
fn preexisting_selfdestruct_is_delete_before_cancun_and_balance_only_after() {
    for spec in all_specs() {
        let mut accounts = account::mock_block_accounts(BLOCK_SIZE);
        let mut bytecodes = HashMap::default();
        let victim_code = selfdestruct_to(receiver());
        let victim_hash = victim_code.hash_slow();
        let mut victim_account = contract_account(&victim_code, VICTIM_BALANCE);
        victim_account.storage.insert(U256::ZERO, U256::from(OLD_STORAGE_VALUE));
        accounts.insert(victim(), victim_account);
        bytecodes.insert(victim_hash, victim_code);
        insert_contract(&mut accounts, &mut bytecodes, probe(), balance_probe(victim()), 0);

        let mut txs = block_txs();
        txs[10] = call_tx(10, victim());
        // A read-only account load after the SELFDESTRUCT result exercises Grevm's deletion
        // barrier.
        txs[50] = call_tx(50, probe());

        let bundle = run_differential(
            spec,
            InMemoryDB::new(accounts, bytecodes, Default::default()),
            txs,
            account::MINER_ADDRESS,
        );
        let final_victim = bundle.state.get(&victim()).expect("victim must have a transition");
        if spec.is_enabled_in(SpecId::CANCUN) {
            let info =
                final_victim.info.as_ref().expect("EIP-6780 preserves a pre-existing account");
            assert_eq!(info.code_hash, victim_hash, "{spec:?}");
            assert_eq!(info.balance, U256::ZERO, "{spec:?}");
            assert!(!final_victim.was_destroyed(), "{spec:?}");
        } else {
            assert!(final_victim.info.is_none(), "{spec:?}");
            assert!(final_victim.was_destroyed(), "{spec:?}");
            assert_eq!(final_victim.storage_slot(U256::ZERO), Some(U256::ZERO), "{spec:?}");
        }
    }
}

#[test]
fn preexisting_selfdestruct_to_self_preserves_balance_only_after_cancun() {
    for spec in all_specs() {
        let mut accounts = account::mock_block_accounts(BLOCK_SIZE);
        let mut bytecodes = HashMap::default();
        let victim_code = selfdestruct_to(victim());
        let victim_hash = victim_code.hash_slow();
        let mut victim_account = contract_account(&victim_code, VICTIM_BALANCE);
        victim_account.storage.insert(U256::ZERO, U256::from(OLD_STORAGE_VALUE));
        accounts.insert(victim(), victim_account);
        bytecodes.insert(victim_hash, victim_code);
        insert_contract(&mut accounts, &mut bytecodes, probe(), balance_probe(victim()), 0);

        let mut txs = block_txs();
        txs[10] = call_tx(10, victim());
        txs[50] = call_tx(50, probe());
        let bundle = run_differential(
            spec,
            InMemoryDB::new(accounts, bytecodes, Default::default()),
            txs,
            account::MINER_ADDRESS,
        );

        let expected_balance = if spec.is_enabled_in(SpecId::CANCUN) { VICTIM_BALANCE } else { 0 };
        let observed_balance = bundle
            .state
            .get(&probe())
            .and_then(|account| account.storage_slot(U256::ZERO))
            .unwrap_or(U256::ZERO);
        assert_eq!(observed_balance, U256::from(expected_balance), "{spec:?}");
        if spec.is_enabled_in(SpecId::CANCUN) {
            // The self-target case is a complete state no-op under EIP-6780.
            if let Some(final_victim) = bundle.state.get(&victim()) {
                let info =
                    final_victim.info.as_ref().expect("EIP-6780 preserves the pre-existing victim");
                assert_eq!(info.balance, U256::from(VICTIM_BALANCE), "{spec:?}");
                assert_eq!(info.code_hash, victim_hash, "{spec:?}");
                assert!(!final_victim.was_destroyed(), "{spec:?}");
            }
        }
    }
}

#[test]
fn created_in_a_prior_transaction_is_not_created_local_at_cancun() {
    for spec in [SpecId::SHANGHAI, SpecId::CANCUN, SpecId::PRAGUE, SpecId::AMSTERDAM] {
        let creator_index = 10;
        let created = account::mock_eoa_address(creator_index).create(1);
        let runtime = increment_then_selfdestruct(receiver());
        let runtime_hash = Bytecode::new_raw(runtime.clone().into()).hash_slow();
        let mut txs = block_txs();
        txs[creator_index] = create_tx(creator_index, deploy_initcode(&runtime));
        txs[30] = call_tx(30, created);
        txs[60] = call_tx(60, created);

        let bundle = run_differential(
            spec,
            InMemoryDB::new(
                account::mock_block_accounts(BLOCK_SIZE),
                Default::default(),
                Default::default(),
            ),
            txs,
            account::MINER_ADDRESS,
        );
        if spec.is_enabled_in(SpecId::CANCUN) {
            let final_created =
                bundle.state.get(&created).expect("preserved created address must be in bundle");
            let info =
                final_created.info.as_ref().expect("cross-transaction SELFDESTRUCT must preserve");
            assert_eq!(info.code_hash, runtime_hash, "{spec:?}");
            assert_eq!(final_created.storage_slot(U256::ZERO), Some(U256::from(2)), "{spec:?}");
            assert!(!final_created.was_destroyed(), "{spec:?}");
        } else {
            // Creation followed by deletion has no net account change and may therefore be omitted
            // from the compacted bundle entirely.
            assert!(
                bundle.state.get(&created).is_none_or(|account| account.info.is_none()),
                "{spec:?}"
            );
        }
    }
}

#[test]
fn create_and_selfdestruct_in_the_same_transaction_always_deletes() {
    for spec in all_specs() {
        let creator_index = 10;
        let created = account::mock_eoa_address(creator_index).create(1);
        let mut txs = block_txs();
        txs[creator_index] =
            create_tx(creator_index, selfdestruct_to(receiver()).original_bytes().to_vec());
        txs[50] = call_tx(50, probe());

        let mut accounts = account::mock_block_accounts(BLOCK_SIZE);
        let mut bytecodes = HashMap::default();
        insert_contract(&mut accounts, &mut bytecodes, probe(), balance_probe(created), 0);
        let bundle = run_differential(
            spec,
            InMemoryDB::new(accounts, bytecodes, Default::default()),
            txs,
            account::MINER_ADDRESS,
        );
        assert!(
            bundle.state.get(&created).is_none_or(|account| account.info.is_none()),
            "{spec:?}: a same-transaction-created account must be absent"
        );
    }
}

#[test]
fn eip161_clears_touched_empty_accounts_but_frontier_materializes_them() {
    for spec in all_specs() {
        let mut accounts = account::mock_block_accounts(BLOCK_SIZE);
        let mut bytecodes = HashMap::default();
        insert_contract(&mut accounts, &mut bytecodes, probe(), balance_probe(missing()), 0);
        let mut txs = block_txs();
        txs[10] = call_tx(10, missing());
        txs[50] = call_tx(50, probe());

        let bundle = run_differential(
            spec,
            InMemoryDB::new(accounts, bytecodes, Default::default()),
            txs,
            account::MINER_ADDRESS,
        );
        let final_info = bundle.state.get(&missing()).and_then(|account| account.info.as_ref());
        if spec.is_enabled_in(SpecId::SPURIOUS_DRAGON) {
            assert!(final_info.is_none(), "{spec:?}: touched empty account must be absent");
        } else {
            assert!(final_info.is_some(), "{spec:?}: Frontier must materialize the empty account");
        }
    }
}

#[test]
fn zero_reward_preserves_revm_touch_semantics_across_eip161() {
    for spec in [SpecId::FRONTIER, SpecId::SPURIOUS_DRAGON, SpecId::PRAGUE] {
        let beneficiary = missing();
        let mut txs = block_txs();
        for tx in &mut txs {
            tx.gas_price = 0;
        }

        let bundle = run_differential(
            spec,
            InMemoryDB::new(
                account::mock_block_accounts(BLOCK_SIZE),
                Default::default(),
                Default::default(),
            ),
            txs,
            beneficiary,
        );
        let final_info = bundle.state.get(&beneficiary).and_then(|account| account.info.as_ref());
        if spec.is_enabled_in(SpecId::SPURIOUS_DRAGON) {
            assert!(final_info.is_none(), "{spec:?}: zero reward must not create an empty account");
        } else {
            assert!(final_info.is_some(), "Frontier zero reward must materialize the beneficiary");
        }
    }
}

#[test]
fn eip161_clears_storage_of_a_preexisting_empty_account() {
    for spec in all_specs() {
        let mut accounts = account::mock_block_accounts(BLOCK_SIZE);
        accounts.insert(
            missing(),
            plain_account(
                AccountInfo {
                    balance: U256::ZERO,
                    nonce: 0,
                    code_hash: KECCAK_EMPTY,
                    code: None,
                    ..Default::default()
                },
                [(U256::ZERO, U256::from(OLD_STORAGE_VALUE))],
            ),
        );
        let mut bytecodes = HashMap::default();
        insert_contract(&mut accounts, &mut bytecodes, probe(), balance_probe(missing()), 0);
        let mut txs = block_txs();
        txs[10] = call_tx(10, missing());
        txs[50] = call_tx(50, probe());

        let bundle = run_differential(
            spec,
            InMemoryDB::new(accounts, bytecodes, Default::default()),
            txs,
            account::MINER_ADDRESS,
        );
        if spec.is_enabled_in(SpecId::SPURIOUS_DRAGON) {
            let final_empty =
                bundle.state.get(&missing()).expect("EIP-161 deletion must be recorded");
            assert!(final_empty.info.is_none(), "{spec:?}");
            assert!(final_empty.was_destroyed(), "{spec:?}");
            assert_eq!(final_empty.storage_slot(U256::ZERO), Some(U256::ZERO), "{spec:?}");
        } else if let Some(final_empty) = bundle.state.get(&missing()) {
            assert!(!final_empty.was_destroyed(), "{spec:?}");
            assert_ne!(final_empty.storage_slot(U256::ZERO), Some(U256::ZERO), "{spec:?}");
        }
    }
}

#[test]
fn reverted_inner_selfdestruct_never_publishes_a_deletion() {
    for spec in all_specs() {
        let mut accounts = account::mock_block_accounts(BLOCK_SIZE);
        let mut bytecodes = HashMap::default();
        let victim_code = selfdestruct_to(receiver());
        let victim_hash = victim_code.hash_slow();
        insert_contract(&mut accounts, &mut bytecodes, victim(), victim_code, VICTIM_BALANCE);
        insert_contract(&mut accounts, &mut bytecodes, parent(), call_then_revert(victim()), 0);
        insert_contract(&mut accounts, &mut bytecodes, probe(), balance_probe(victim()), 0);
        let mut txs = block_txs();
        txs[10] = call_tx(10, parent());
        txs[50] = call_tx(50, probe());

        let bundle = run_differential(
            spec,
            InMemoryDB::new(accounts, bytecodes, Default::default()),
            txs,
            account::MINER_ADDRESS,
        );
        if let Some(final_victim) = bundle.state.get(&victim()) {
            let info =
                final_victim.info.as_ref().expect("reverted SELFDESTRUCT must preserve victim");
            assert_eq!(info.code_hash, victim_hash, "{spec:?}");
            assert_eq!(info.balance, U256::from(VICTIM_BALANCE), "{spec:?}");
            assert!(!final_victim.was_destroyed(), "{spec:?}");
        }
    }
}

#[test]
fn beneficiary_actual_selfdestruct_dominates_the_transaction_reward() {
    let spec = SpecId::SHANGHAI;
    let mut accounts = account::mock_block_accounts(BLOCK_SIZE);
    let mut bytecodes = HashMap::default();
    insert_contract(
        &mut accounts,
        &mut bytecodes,
        victim(),
        selfdestruct_to(receiver()),
        VICTIM_BALANCE,
    );
    let mut txs = block_txs();
    // Keep the deletion last: rewards from later transactions would legitimately materialize the
    // beneficiary again, while this transaction's own reward is still suppressed by its deletion.
    txs[BLOCK_SIZE - 1] = call_tx(BLOCK_SIZE - 1, victim());

    let bundle = run_differential(
        spec,
        InMemoryDB::new(accounts, bytecodes, Default::default()),
        txs,
        victim(),
    );
    let final_beneficiary =
        bundle.state.get(&victim()).expect("beneficiary deletion must be recorded");
    assert!(final_beneficiary.info.is_none());
    assert!(final_beneficiary.was_destroyed());
}

#[test]
fn cancun_created_beneficiary_actual_selfdestruct_dominates_reward() {
    // Put Amsterdam first so a failure cannot be hidden behind an earlier fork: EIP-7708 also
    // makes the reward/deletion ordering observable in the transaction logs.
    for spec in [SpecId::AMSTERDAM, SpecId::CANCUN, SpecId::PRAGUE] {
        let creator_index = BLOCK_SIZE - 1;
        let beneficiary = account::mock_eoa_address(creator_index).create(1);
        let mut txs = block_txs();
        txs[creator_index] =
            create_tx(creator_index, selfdestruct_to(receiver()).original_bytes().to_vec());

        let bundle = run_differential(
            spec,
            InMemoryDB::new(
                account::mock_block_accounts(BLOCK_SIZE),
                Default::default(),
                Default::default(),
            ),
            txs,
            beneficiary,
        );
        assert!(
            bundle.state.get(&beneficiary).is_none_or(|account| account.info.is_none()),
            "{spec:?}: actual SELFDESTRUCT must dominate deferred beneficiary reward"
        );
    }
}
