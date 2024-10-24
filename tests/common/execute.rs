use crate::common::{storage::InMemoryDB, MINER_ADDRESS};
use alloy_rpc_types::Block;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};

use alloy_chains::NamedChain;
use grevm::{ExecuteOutput, GrevmScheduler};
use revm::{
    db::{
        states::{bundle_state::BundleRetention, StorageSlot},
        BundleAccount, BundleState, PlainAccount,
    },
    primitives::{
        alloy_primitives::U160, uint, AccountInfo, Address, Bytecode, EVMError, Env,
        ExecutionResult, SpecId, TxEnv, B256, KECCAK_EMPTY, U256,
    },
    DatabaseCommit, DatabaseRef, EvmBuilder, StateBuilder,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    fs::{self, File},
    io::BufReader,
    sync::Arc,
    time::Instant,
};

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
        let left_storage: BTreeMap<&U256, &StorageSlot> = account1.storage.iter().collect();
        let right_storage: BTreeMap<&U256, &StorageSlot> = account2.storage.iter().collect();
        for (s1, s2) in left_storage.into_iter().zip(right_storage.into_iter()) {
            assert_eq!(s1, s2, "Address: {:?}", addr1);
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
    parallel_metrics: HashMap<&str, DebugValue>,
) where
    DB: DatabaseRef + Send + Sync + 'static,
    DB::Error: Send + Sync + Clone + Debug,
{
    // create registry for metrics
    let recorder = DebuggingRecorder::new();

    let mut env = Env::default();
    env.cfg.chain_id = NamedChain::Mainnet.into();
    env.block.coinbase = Address::from(U160::from(MINER_ADDRESS));
    let db = Arc::new(db);
    let txs = Arc::new(txs);
    let start = Instant::now();
    let sequential = GrevmScheduler::new(SpecId::LATEST, env.clone(), db.clone(), txs.clone());
    let sequential_result = sequential.force_sequential_execute();
    println!("Grevm sequential execute time: {}ms", start.elapsed().as_millis());

    let parallel_result = metrics::with_local_recorder(&recorder, || {
        let start = Instant::now();
        let parallel = GrevmScheduler::new(SpecId::LATEST, env.clone(), db.clone(), txs.clone());
        // set determined partitions
        let parallel_result = parallel.force_parallel_execute(with_hints, Some(23));
        println!("Grevm parallel execute time: {}ms", start.elapsed().as_millis());

        let snapshot = recorder.snapshotter().snapshot();
        for (key, unit, desc, value) in snapshot.into_vec() {
            println!("metrics: {} => value: {:?}", key.key().name(), value);
            if let Some(metric) = parallel_metrics.get(key.key().name()) {
                assert_eq!(*metric, value);
            }
        }
        parallel_result
    });

    let start = Instant::now();
    let reth_result = execute_revm_sequential(db.clone(), SpecId::LATEST, env.clone(), &*txs);
    println!("Origin sequential execute time: {}ms", start.elapsed().as_millis());

    let mut max_gas_spent = 0;
    let mut max_gas_used = 0;
    for result in &reth_result.as_ref().unwrap().results {
        match result {
            ExecutionResult::Success { gas_used, gas_refunded, .. } => {
                max_gas_spent = max_gas_spent.max(gas_used + gas_refunded);
                max_gas_used = max_gas_used.max(*gas_used);
            }
            _ => panic!("result is not success"),
        }
    }
    println!("max_gas_spent: {}, max_gas_used: {}", max_gas_spent, max_gas_used);

    compare_execution_result(
        &reth_result.as_ref().unwrap().results,
        &sequential_result.as_ref().unwrap().results,
    );
    compare_execution_result(
        &reth_result.as_ref().unwrap().results,
        &parallel_result.as_ref().unwrap().results,
    );

    compare_bundle_state(
        &reth_result.as_ref().unwrap().state,
        &sequential_result.as_ref().unwrap().state,
    );
    compare_bundle_state(
        &reth_result.as_ref().unwrap().state,
        &parallel_result.as_ref().unwrap().state,
    );
}

/// Simulate the sequential execution of transactions in reth
pub(crate) fn execute_revm_sequential<DB>(
    db: DB,
    spec_id: SpecId,
    env: Env,
    txs: &[TxEnv],
) -> Result<ExecuteOutput, EVMError<DB::Error>>
where
    DB: DatabaseRef,
    DB::Error: Debug,
{
    let db = StateBuilder::new()
        .with_bundle_update()
        .without_state_clear()
        .with_database_ref(db)
        .build();
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

    Ok(ExecuteOutput { state: evm.db_mut().take_bundle(), results })
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct JsonAccount {
    /// The account's balance.
    pub balance: U256,
    /// The account's nonce.
    pub nonce: u64,
    /// The optional code hash of the account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_hash: Option<B256>,
    /// The account's storage.
    pub storage: HashMap<U256, U256>,
}

const TEST_DATA_DIR: &str = "test_data";

pub(crate) fn load_bytecodes_from_disk() -> HashMap<B256, Bytecode> {
    // Parse bytecodes
    bincode::deserialize_from(BufReader::new(
        File::open(format!("{TEST_DATA_DIR}/bytecodes.bincode")).unwrap(),
    ))
    .unwrap()
}

pub(crate) fn load_block_from_disk(
    block_number: u64,
) -> (Block, HashMap<Address, PlainAccount>, HashMap<u64, B256>) {
    // Parse block
    let block: Block = serde_json::from_reader(BufReader::new(
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/block.json")).unwrap(),
    ))
    .unwrap();

    // Parse state
    let accounts: HashMap<Address, JsonAccount> = serde_json::from_reader(BufReader::new(
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/pre_state.json")).unwrap(),
    ))
    .unwrap();
    let accounts: HashMap<Address, PlainAccount> = accounts
        .into_iter()
        .map(|(address, account)| {
            let account = PlainAccount {
                info: AccountInfo {
                    balance: account.balance,
                    nonce: account.nonce,
                    code_hash: account.code_hash.unwrap_or_else(|| KECCAK_EMPTY),
                    code: None,
                },
                storage: account.storage,
            };
            (address, account)
        })
        .collect();

    // Parse block hashes
    let block_hashes: HashMap<u64, B256> =
        File::open(format!("{TEST_DATA_DIR}/blocks/{block_number}/block_hashes.json"))
            .map(|file| {
                serde_json::from_reader::<_, HashMap<u64, B256>>(BufReader::new(file)).unwrap()
            })
            .unwrap_or_default();

    (block, accounts, block_hashes)
}

pub(crate) fn for_each_block_from_disk(mut handler: impl FnMut(Block, InMemoryDB)) {
    // Parse bytecodes
    let bytecodes = load_bytecodes_from_disk();

    for block_path in fs::read_dir(format!("{TEST_DATA_DIR}/blocks")).unwrap() {
        let block_path = block_path.unwrap().path();
        let block_number = block_path.file_name().unwrap().to_str().unwrap();

        let (block, accounts, block_hashes) = load_block_from_disk(block_number.parse().unwrap());
        handler(block, InMemoryDB::new(accounts, bytecodes.clone(), block_hashes));
    }
}
