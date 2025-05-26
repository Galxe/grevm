use crate::common::{storage::InMemoryDB, MINER_ADDRESS};
use alloy_primitives::address;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};

use alloy_chains::NamedChain;

use alloy_sol_macro::sol;
use alloy_sol_types::{SolCall, SolConstructor};
use grevm::{ParallelState, ParallelTakeBundle, Scheduler};
use revm::{
    db::{
        states::{bundle_state::BundleRetention, StorageSlot},
        AccountRevert, BundleAccount, BundleState, PlainAccount,
    },
    primitives::{
        alloy_primitives::U160, uint, AccountInfo, Address, Bytecode, EVMError, Env,
        ExecutionResult, SpecId, TxEnv, B256, KECCAK_EMPTY, U256,
    },
    CacheState, DatabaseCommit, DatabaseRef, EvmBuilder, StateBuilder, TransitionState,
};
use revm_primitives::{hex, keccak256, Bytes, EnvWithHandlerCfg, ResultAndState, TxKind};
use std::{
    cmp::min,
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    fs::{self, File},
    io::{BufReader, BufWriter},
    sync::Arc,
    time::Instant,
};

pub(crate) fn compare_result_and_state(left: &Vec<ResultAndState>, right: &Vec<ResultAndState>) {
    assert_eq!(left.len(), right.len());
    for (l, r) in left.iter().zip(right.iter()) {}
    for txid in 0..left.len() {
        assert_eq!(left[txid].rewards, right[txid].rewards, "Tx {}", txid);
        assert_eq!(left[txid].result, right[txid].result, "Tx {}", txid);
        let l = &left[txid].state;
        let r = &right[txid].state;
        for (address, l) in l {
            let r =
                r.get(address).expect(format!("no account {:?} in Tx {}", address, txid).as_str());
            assert_eq!(l.info, r.info, "account {:?} info in Tx {}", address, txid);
            assert_eq!(l.storage, r.storage, "account {:?} storage in Tx {}", address, txid);
            assert_eq!(l.status, r.status, "account {:?} status in Tx {}", address, txid);
        }
    }
}

pub(crate) fn compare_bundle_state(left: &BundleState, right: &BundleState) {
    assert!(
        left.contracts.keys().all(|k| right.contracts.contains_key(k)),
        "Left contracts: {:?}, Right contracts: {:?}",
        left.contracts.keys(),
        right.contracts.keys()
    );
    assert_eq!(
        left.contracts.len(),
        right.contracts.len(),
        "Left contracts: {:?}, Right contracts: {:?}",
        left.contracts.keys(),
        right.contracts.keys()
    );

    let left_state: BTreeMap<&Address, &BundleAccount> = left.state.iter().collect();
    let right_state: BTreeMap<&Address, &BundleAccount> = right.state.iter().collect();
    assert_eq!(left_state.len(), right_state.len());

    for ((addr1, account1), (addr2, account2)) in
        left_state.into_iter().zip(right_state.into_iter())
    {
        assert_eq!(addr1, addr2);
        assert_eq!(account1.info, account2.info, "Address: {:?}", addr1);
        assert_eq!(account1.original_info, account2.original_info, "Address: {:?}", addr1);
        assert_eq!(account1.status, account2.status, "Address: {:?}", addr1);
        assert_eq!(account1.storage.len(), account2.storage.len());
        let left_storage: BTreeMap<&U256, &StorageSlot> = account1.storage.iter().collect();
        let right_storage: BTreeMap<&U256, &StorageSlot> = account2.storage.iter().collect();
        for (s1, s2) in left_storage.into_iter().zip(right_storage.into_iter()) {
            assert_eq!(s1, s2, "Address: {:?}", addr1);
        }
    }

    assert_eq!(left.reverts.len(), right.reverts.len());
    for (left, right) in left.reverts.iter().zip(right.reverts.iter()) {
        assert_eq!(left.len(), right.len());
        let right: HashMap<&Address, &AccountRevert> = right.iter().map(|(k, v)| (k, v)).collect();
        for (addr, revert) in left.iter() {
            assert_eq!(revert, *right.get(addr).unwrap(), "Address: {:?}", addr);
        }
    }
}

pub(crate) fn compare_execution_result(left: &Vec<ExecutionResult>, right: &Vec<ExecutionResult>) {
    for (i, (left_res, right_res)) in left.iter().zip(right.iter()).enumerate() {
        assert_eq!(left_res, right_res, "Tx {}", i);
    }
    assert_eq!(left.len(), right.len());
}

pub(crate) fn mock_miner_account() -> (Address, PlainAccount) {
    let address = Address::from(U160::from(MINER_ADDRESS));
    let account = PlainAccount {
        info: AccountInfo { balance: U256::from(0), nonce: 1, code_hash: KECCAK_EMPTY, code: None },
        storage: Default::default(),
    };
    (address, account)
}

pub(crate) fn mock_eoa_account(idx: usize) -> (Address, PlainAccount) {
    let address = Address::from(U160::from(idx));
    let account = PlainAccount {
        info: AccountInfo {
            balance: uint!(1_000_000_000_000_000_000_U256),
            nonce: 1,
            code_hash: KECCAK_EMPTY,
            code: None,
        },
        storage: Default::default(),
    };
    (address, account)
}

pub(crate) fn mock_block_accounts(from: usize, size: usize) -> HashMap<Address, PlainAccount> {
    let mut accounts: HashMap<Address, PlainAccount> =
        (from..(from + size)).map(mock_eoa_account).collect();
    let miner = mock_miner_account();
    accounts.insert(miner.0, miner.1);
    accounts
}

pub(crate) fn compare_evm_execute<DB>(
    db: DB,
    txs: Vec<TxEnv>,
    with_hints: bool,
    parallel_metrics: HashMap<&str, usize>,
) where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Send + Sync + Clone + Debug,
{
    // create registry for metrics
    let recorder = DebuggingRecorder::new();

    let mut env = Env::default();
    env.cfg.chain_id = NamedChain::Mainnet.into();
    env.block.coinbase = Address::from(U160::from(MINER_ADDRESS));
    let db = Arc::new(db);
    let txs = Arc::new(txs);

    let parallel_result = metrics::with_local_recorder(&recorder, || {
        let start = Instant::now();
        let state = ParallelState::new(db.clone(), true, true);
        let mut executor =
            Scheduler::new(SpecId::LATEST, env.clone(), txs.clone(), state, with_hints);
        // set determined partitions
        executor.parallel_execute(Some(23)).expect("parallel execute failed");
        println!("Grevm parallel execute time: {}ms", start.elapsed().as_millis());

        let snapshot = recorder.snapshotter().snapshot();
        for (key, _, _, value) in snapshot.into_vec() {
            let value = match value {
                DebugValue::Counter(v) => v as usize,
                DebugValue::Gauge(v) => v.0 as usize,
                DebugValue::Histogram(v) => v.last().cloned().map_or(0, |ov| ov.0 as usize),
            };
            println!("metrics: {} => value: {:?}", key.key().name(), value);
            if let Some(metric) = parallel_metrics.get(key.key().name()) {
                assert_eq!(*metric, value);
            }
        }
        let (results, mut state) = executor.take_result_and_state();
        (results, state.parallel_take_bundle(BundleRetention::Reverts))
    });

    let start = Instant::now();
    let reth_result =
        execute_revm_sequential(db.clone(), SpecId::LATEST, env.clone(), &*txs).unwrap();
    println!("Origin sequential execute time: {}ms", start.elapsed().as_millis());

    let mut max_gas_spent = 0;
    let mut max_gas_used = 0;
    for result in reth_result.0.iter() {
        match result {
            ExecutionResult::Success { gas_used, gas_refunded, .. } => {
                max_gas_spent = max_gas_spent.max(gas_used + gas_refunded);
                max_gas_used = max_gas_used.max(*gas_used);
            }
            _ => panic!("result is not success"),
        }
    }
    println!("max_gas_spent: {}, max_gas_used: {}", max_gas_spent, max_gas_used);
    compare_execution_result(&reth_result.0, &parallel_result.0);
    compare_bundle_state(&reth_result.1, &parallel_result.1);
}

/// Simulate the sequential execution of transactions in reth
pub(crate) fn execute_revm_sequential<DB>(
    db: DB,
    spec_id: SpecId,
    env: Env,
    txs: &[TxEnv],
) -> Result<(Vec<ExecutionResult>, BundleState), EVMError<DB::Error>>
where
    DB: DatabaseRef,
    DB::Error: Debug,
{
    let db = StateBuilder::new().with_bundle_update().with_database_ref(db).build();
    let mut evm =
        EvmBuilder::default().with_db(db).with_spec_id(spec_id).with_env(Box::new(env)).build();

    let mut results = Vec::with_capacity(txs.len());
    for tx in txs {
        *evm.tx_mut() = tx.clone();
        let result_and_state = evm.transact()?;
        evm.db_mut().commit(result_and_state.state);
        results.push(result_and_state.result);
    }
    evm.db_mut().merge_transitions(BundleRetention::Reverts);

    Ok((results, evm.db_mut().take_bundle()))
}

const TEST_DATA_DIR: &str = "test_data";

pub(crate) fn load_bytecodes_from_disk() -> HashMap<B256, Bytecode> {
    // Parse bytecodes
    bincode::deserialize_from(BufReader::new(
        File::open(format!("{TEST_DATA_DIR}/bytecodes.bincode")).unwrap(),
    ))
    .unwrap()
}

pub(crate) fn continuous_blocks_exist(blocks: String) -> bool {
    if let Ok(exist) = fs::exists(format!("{TEST_DATA_DIR}/con_eth_blocks/{blocks}")) {
        exist
    } else {
        false
    }
}

pub(crate) fn load_continuous_blocks(
    blocks: String,
    num_blocks: Option<usize>,
) -> (Vec<(EnvWithHandlerCfg, Vec<TxEnv>, HashMap<Address, u128>)>, InMemoryDB) {
    let splits: Vec<&str> = blocks.split('_').collect();
    let start_block: usize = splits[0].parse().unwrap();
    let mut end_block: usize = splits[1].parse::<usize>().unwrap() + 1;
    if let Some(num_blocks) = num_blocks {
        end_block = min(end_block, start_block + num_blocks);
    }
    let mut block_txs = Vec::with_capacity(end_block - start_block);
    for block_number in start_block..end_block {
        let env: EnvWithHandlerCfg = serde_json::from_reader(BufReader::new(
            File::open(format!("{TEST_DATA_DIR}/con_eth_blocks/{blocks}/{block_number}/env.json"))
                .unwrap(),
        ))
        .unwrap();
        let txs: Vec<TxEnv> = serde_json::from_reader(BufReader::new(
            File::open(format!("{TEST_DATA_DIR}/con_eth_blocks/{blocks}/{block_number}/txs.json"))
                .unwrap(),
        ))
        .unwrap();
        let post: HashMap<Address, u128> = serde_json::from_reader(BufReader::new(
            File::open(format!("{TEST_DATA_DIR}/con_eth_blocks/{blocks}/{block_number}/post.json"))
                .unwrap(),
        ))
        .unwrap();
        block_txs.push((env, txs, post));
    }

    // Parse state
    let accounts: HashMap<Address, PlainAccount> = serde_json::from_reader(BufReader::new(
        File::open(format!("{TEST_DATA_DIR}/con_eth_blocks/{blocks}/pre_state.json")).unwrap(),
    ))
    .unwrap();

    // Parse block hashes
    let block_hashes: HashMap<u64, B256> =
        File::open(format!("{TEST_DATA_DIR}/con_eth_blocks/{blocks}/block_hashes.json"))
            .map(|file| serde_json::from_reader(BufReader::new(file)).unwrap())
            .unwrap_or_default();

    // Parse bytecodes
    let bytecodes: HashMap<B256, Bytecode> =
        File::open(format!("{TEST_DATA_DIR}/con_eth_blocks/{blocks}/bytecodes.bincode"))
            .map(|file| bincode::deserialize_from(BufReader::new(file)).unwrap())
            .unwrap_or_default();

    (block_txs, InMemoryDB::new(accounts, bytecodes, block_hashes))
}

pub(crate) fn load_block_from_disk(
    block_number: u64,
) -> (EnvWithHandlerCfg, Vec<TxEnv>, InMemoryDB) {
    // Parse block
    let env: EnvWithHandlerCfg = serde_json::from_reader(BufReader::new(
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/env.json")).unwrap(),
    ))
    .unwrap();
    let txs: Vec<TxEnv> = serde_json::from_reader(BufReader::new(
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/txs.json")).unwrap(),
    ))
    .unwrap();

    // Parse state
    let accounts: HashMap<Address, PlainAccount> = serde_json::from_reader(BufReader::new(
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/pre_state.json")).unwrap(),
    ))
    .unwrap();

    // Parse block hashes
    let block_hashes: HashMap<u64, B256> =
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/block_hashes.json"))
            .map(|file| serde_json::from_reader(BufReader::new(file)).unwrap())
            .unwrap_or_default();

    // Parse bytecodes
    let bytecodes: HashMap<B256, Bytecode> =
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/bytecodes.bincode"))
            .map(|file| bincode::deserialize_from(BufReader::new(file)).unwrap())
            .unwrap_or_default();

    (env, txs, InMemoryDB::new(accounts, bytecodes, block_hashes))
}

pub(crate) fn for_each_block_from_disk(
    mut handler: impl FnMut(EnvWithHandlerCfg, Vec<TxEnv>, InMemoryDB),
) {
    // Parse bytecodes
    let bytecodes = load_bytecodes_from_disk();

    for block_path in fs::read_dir(format!("{TEST_DATA_DIR}/blocks")).unwrap() {
        let block_path = block_path.unwrap().path();
        let block_number = block_path.file_name().unwrap().to_str().unwrap();

        let (env, txs, mut db) = load_block_from_disk(block_number.parse().unwrap());
        if db.bytecodes.is_empty() {
            // Use the global bytecodes if the block doesn't have its own
            db.bytecodes = bytecodes.clone();
        }
        handler(env, txs, db);
    }
}

pub(crate) fn dump_block_env(
    env: &EnvWithHandlerCfg,
    txs: &[TxEnv],
    cache_state: &CacheState,
    transition_state: &TransitionState,
    block_hashes: &BTreeMap<u64, B256>,
) {
    let path = format!("{}/large_block", TEST_DATA_DIR);

    // Write env data to file
    serde_json::to_writer(BufWriter::new(File::create(format!("{path}/env.json")).unwrap()), env)
        .unwrap();

    // Write txs data to file
    serde_json::to_writer(BufWriter::new(File::create(format!("{path}/txs.json")).unwrap()), txs)
        .unwrap();

    // Write pre-state and bytecodes data to file
    let mut pre_state: HashMap<Address, PlainAccount> =
        HashMap::with_capacity(transition_state.transitions.len());
    let mut bytecodes = cache_state.contracts.clone();
    for (addr, account) in cache_state.accounts.iter() {
        if let Some(transition_account) = transition_state.transitions.get(addr) {
            // account has been modified by execution, use previous info
            if let Some(info) = transition_account.previous_info.as_ref() {
                let mut storage = if let Some(account) = account.account.as_ref() {
                    account.storage.clone()
                } else {
                    HashMap::default()
                };
                storage.extend(
                    transition_account.storage.iter().map(|(k, v)| (*k, v.original_value())),
                );

                let mut info = info.clone();
                if let Some(code) = info.code.take() {
                    bytecodes.entry(info.code_hash).or_insert_with(|| code);
                }
                pre_state.insert(*addr, PlainAccount { info, storage });
            }
        } else if let Some(account) = account.account.as_ref() {
            // account has not been modified, use current info in cache
            let mut account = account.clone();
            if let Some(code) = account.info.code.take() {
                bytecodes.entry(account.info.code_hash).or_insert_with(|| code);
            }
            pre_state.insert(*addr, account.clone());
        }
    }

    serde_json::to_writer(
        BufWriter::new(File::create(format!("{path}/pre_state.json")).unwrap()),
        &pre_state,
    )
    .unwrap();
    bincode::serialize_into(
        BufWriter::new(File::create(format!("{path}/bytecodes.bincode")).unwrap()),
        &bytecodes,
    )
    .unwrap();

    // Write block hashes to file
    serde_json::to_writer(
        BufWriter::new(File::create(format!("{path}/block_hashes.json")).unwrap()),
        block_hashes,
    )
    .unwrap();
}

const SYSTEM_ADDRESS: Address = address!("00000000000000000000000000000000000000ff");

fn new_system_call_txn(contract: Address, input: Bytes) -> TxEnv {
    TxEnv {
        caller: SYSTEM_ADDRESS,
        gas_limit: 30_000_000,
        gas_price: U256::ZERO,
        transact_to: TxKind::Call(contract),
        value: U256::ZERO,
        data: input,
        ..Default::default()
    }
}

fn new_system_create_txn(hex_code: &str, args: Bytes) -> TxEnv {
    let mut data = hex::decode(hex_code).expect("Invalid hex string");
    data.extend_from_slice(&args);
    TxEnv {
        caller: SYSTEM_ADDRESS,
        gas_limit: 30_000_000,
        gas_price: U256::ZERO,
        transact_to: TxKind::Create,
        value: U256::ZERO,
        data: data.into(),
        ..Default::default()
    }
}

#[test]
fn genesis_test() {
    let mut env = Env::default();
    env.cfg.chain_id = NamedChain::Mainnet.into();
    let db = InMemoryDB::new(
        HashMap::from([(
            SYSTEM_ADDRESS,
            PlainAccount {
                info: AccountInfo {
                    balance: uint!(1_000_000_000_000_000_000_U256),
                    nonce: 1,
                    code_hash: KECCAK_EMPTY,
                    code: None,
                },
                storage: Default::default(),
            },
        )]),
        Default::default(),
        Default::default(),
    );

    let reconfig_sol_hex = std::io::read_to_string(
        File::open(format!("{TEST_DATA_DIR}/gravity/reconfig.sol.hex"))
            .expect("Failed to open reconfig.sol.hex"),
    )
    .unwrap();
    let block_sol_hex = std::io::read_to_string(
        File::open(format!("{TEST_DATA_DIR}/gravity/block.sol.hex"))
            .expect("Failed to open block.sol.hex"),
    )
    .unwrap();
    let consensus_config_sol_hex = std::io::read_to_string(
        File::open(format!("{TEST_DATA_DIR}/gravity/consensus_config.sol.hex"))
            .expect("Failed to open consensus.sol.hex"),
    )
    .unwrap();

    let reconfig_address = SYSTEM_ADDRESS.create(1);
    let block_address = SYSTEM_ADDRESS.create(2);
    let consensus_config_address = SYSTEM_ADDRESS.create(3);

    let mut txs = Vec::new();

    sol! {
        contract Reconfiguration {
            constructor(
                address _aptosFrameworkAddress,
                address _authorizedBlockOrGovManager
            );

            // --- Functions equivalent to Move module functions ---

            /// @notice Globally disables the reconfiguration process.
            /// Should only be used for specific system-level operations.
            function disableReconfigurationGlobally() external onlyAptosFramework;

            /// @notice Globally enables the reconfiguration process.
            function enableReconfigurationGlobally() external onlyAptosFramework;

            /// @notice Checks if reconfiguration is currently enabled.
            function isReconfigurationEnabled() public view returns (bool);

            /// @notice Signals validators to start using new configuration.
            /// @param currentTimeMicroseconds The current time in microseconds, expected to be provided by the caller.
            function reconfigure(uint64 currentTimeMicroseconds) external onlyAuthorizedManager;

            /// @notice Returns the time of the last reconfiguration in microseconds.
            function getLastReconfigurationTime() external view returns (uint64);

            /// @notice Returns the current epoch number.
            function getCurrentEpoch() external view returns (uint64);

            /// @notice Emits the very first reconfiguration event during genesis.
            /// Typically called once by the system setup/genesis process.
            function emitGenesisReconfigurationEvent() external onlyAptosFramework;

            function setAuthorizedBlockOrGovManager(address _newManager) external onlyAptosFramework;
        }
    }

    txs.push(new_system_create_txn(
        &reconfig_sol_hex,
        Reconfiguration::constructorCall {
            _aptosFrameworkAddress: SYSTEM_ADDRESS,
            _authorizedBlockOrGovManager: SYSTEM_ADDRESS,
        }
        .abi_encode()
        .into(),
    ));
    println!("Reconfiguration contract address: {:?}", reconfig_address);
    txs.push(new_system_call_txn(
        reconfig_address,
        Reconfiguration::emitGenesisReconfigurationEventCall {}.abi_encode().into(),
    ));

    sol! {
        contract BlockModule {
            constructor(
                uint64 _initial_epoch_interval_microsecs,
                address _vmAddress,
                address _reconfigurationContractAddress
            );
        }
    }

    txs.push(new_system_create_txn(
        &block_sol_hex,
        BlockModule::constructorCall {
            _initial_epoch_interval_microsecs: 10_000_000,
            _vmAddress: SYSTEM_ADDRESS,
            _reconfigurationContractAddress: address!("00000000000000000000000000000000000000f0"),
        }
        .abi_encode()
        .into(),
    ));

    sol! {
        contract ConsensusConfigContract {
            constructor(
                address _aptosFrameworkAddress,
                address _reconfigurationContractAddress, // For calling reconfigure()
                bytes memory initialConfig
            );
        }
    }

    txs.push(new_system_create_txn(
        &consensus_config_sol_hex,
        ConsensusConfigContract::constructorCall {
            _aptosFrameworkAddress: SYSTEM_ADDRESS,
            _reconfigurationContractAddress: address!("00000000000000000000000000000000000000f0"),
            initialConfig: Bytes::from([
                3, 1, 1, 10, 0, 0, 0, 0, 0, 0, 0, 40, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 10, 0, 0,
                0, 0, 0, 0, 0, 1, 0, 0, 0,
            ]),
        }
        .abi_encode()
        .into(),
    ));

    let (result, mut bundle_state) =
        execute_revm_sequential(db, SpecId::LATEST, env, &txs).unwrap();
    for r in result {
        assert!(r.is_success(), "Genesis transaction failed: {:?}", r);
        println!("Genesis transaction result: {:?}", r);
    }
    //println!("Bundle state: {:?}", bundle_state.state);
    bundle_state.state.remove(&SYSTEM_ADDRESS);
    let genesis_state = bundle_state
        .state
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                PlainAccount {
                    info: v.info.unwrap(),
                    storage: v.storage.into_iter().map(|(k, v)| (k, v.present_value())).collect(),
                },
            )
        })
        .collect::<HashMap<_, _>>();
    serde_json::to_writer_pretty(
        BufWriter::new(
            File::create(format!("{TEST_DATA_DIR}/gravity/genesis_accounts.json")).unwrap(),
        ),
        &genesis_state,
    )
    .unwrap();
    serde_json::to_writer_pretty(
        BufWriter::new(
            File::create(format!("{TEST_DATA_DIR}/gravity/genesis_contracts.json")).unwrap(),
        ),
        &bundle_state.contracts.iter().map(|(k, v)| (k, v.bytecode())).collect::<HashMap<_, _>>(),
    )
    .unwrap();
}
