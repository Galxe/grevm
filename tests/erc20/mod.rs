use std::collections::HashMap;

use revm::db::{DbAccount, PlainAccount};
use revm::interpreter::analysis::to_analysed;
use revm::primitives::{uint, AccountInfo, Address, Bytecode, TransactTo, TxEnv, B256, U256};

use crate::erc20::erc20_contract::ERC20Token;

pub mod erc20_contract;

pub const GAS_LIMIT: u64 = 35_000;
pub const ESTIMATED_GAS_USED: u64 = 29_738;

/// Mapping from address to [EvmAccount]
pub type ChainState = HashMap<Address, DbAccount>;

/// Mapping from code hashes to [EvmCode]s
pub type Bytecodes = HashMap<B256, Bytecode>;

/// Mapping from block numbers to block hashes
pub type BlockHashes = HashMap<u64, B256>;

// TODO: Better randomness control. Sometimes we want duplicates to test
// dependent transactions, sometimes we want to guarantee non-duplicates
// for independent benchmarks.
fn generate_addresses(length: usize) -> Vec<Address> {
    (0..length).map(|_| Address::new(rand::random())).collect()
}

#[derive(Clone, Debug, Copy)]
pub enum TransactionCallDataType {
    Empty,
    Mix,
    Allowance,
    Approve,
    BalanceOf,
    Decimals,
    DecreaseAllowance,
    IncreaseAllowance,
    Name,
    Symbol,
    TotalSupply,
    Transfer,
    TransferFrom,
}

#[derive(Clone, Debug, Copy)]
pub enum TransactionModeType {
    Random,
    SameCaller,
    Empty,
}

#[derive(Clone, Debug)]
pub struct TxnBatchConfig {
    num_eoa: usize,
    num_sca: usize,
    num_txns_per_address: usize,
    transaction_call_data_type: TransactionCallDataType,
    transaction_mode_type: TransactionModeType,
}

impl TxnBatchConfig {
    pub fn new(
        num_eoa: usize,
        num_sca: usize,
        num_txns_per_address: usize,
        transaction_call_data_type: TransactionCallDataType,
        transaction_mode_type: TransactionModeType,
    ) -> Self {
        TxnBatchConfig {
            num_eoa,
            num_sca,
            num_txns_per_address,
            transaction_call_data_type,
            transaction_mode_type,
        }
    }
}

pub fn generate_erc20_batch(
    eoa_addresses: Vec<Address>,
    sca_addresses: Vec<Address>,
    num_transfers_per_eoa: usize,
    transaction_call_data_type: TransactionCallDataType,
    transaction_mode_type: TransactionModeType,
) -> Vec<TxEnv> {
    let mut txns = Vec::new();

    for nonce in 0..num_transfers_per_eoa {
        for caller_index in 0..eoa_addresses.len() {
            let sender: Address = eoa_addresses[caller_index];

            let recipient: Address = match transaction_mode_type {
                TransactionModeType::SameCaller => sender,
                TransactionModeType::Random => {
                    eoa_addresses[rand::random::<usize>() % eoa_addresses.len()]
                }
                TransactionModeType::Empty => Address::new([0; 20]),
            };
            let to_address: Address = sca_addresses[rand::random::<usize>() % sca_addresses.len()];

            let mut tx_env = TxEnv {
                caller: sender,
                gas_limit: GAS_LIMIT,
                gas_price: U256::from(0xb2d05e07u64),
                transact_to: TransactTo::Call(to_address),
                value: U256::from(0),
                nonce: Some(nonce as u64),
                ..TxEnv::default()
            };

            match transaction_call_data_type {
                TransactionCallDataType::Transfer => {
                    tx_env.data = ERC20Token::transfer(recipient, U256::from(rand::random::<u8>()));
                }
                TransactionCallDataType::TransferFrom => {
                    tx_env.data = ERC20Token::transfer_from(
                        sender,
                        recipient,
                        U256::from(rand::random::<u8>()),
                    );
                }
                _ => {}
            }
            txns.push(tx_env);
        }
    }
    txns
}

/// Return a tuple of (state, bytecodes, eoa_addresses, sca_addresses)
pub fn generate_cluster(
    num_eoa: usize,
    num_sca: usize,
) -> (HashMap<Address, PlainAccount>, HashMap<B256, Bytecode>, Vec<Address>, Vec<Address>) {
    let mut state = HashMap::new();
    let eoa_addresses: Vec<Address> = generate_addresses(num_eoa);

    for person in eoa_addresses.iter() {
        state.insert(
            *person,
            PlainAccount {
                info: AccountInfo {
                    balance: uint!(1_000_000_000_000_000_000_U256),
                    ..AccountInfo::default()
                },
                ..PlainAccount::default()
            },
        );
    }

    let (contract_accounts, bytecodes) = generate_contract_accounts(num_sca, &eoa_addresses);
    let mut erc20_sca_addresses = Vec::with_capacity(num_sca);
    for (addr, sca) in contract_accounts {
        state.insert(addr, sca);
        erc20_sca_addresses.push(addr);
    }

    (state, bytecodes, eoa_addresses, erc20_sca_addresses)
}

pub fn generate_cluster_and_txs(
    txn_batch_config: &TxnBatchConfig,
) -> (HashMap<Address, PlainAccount>, HashMap<B256, Bytecode>, Vec<TxEnv>) {
    let (state, bytecodes, eoa_addresses, erc20_sca_address) =
        generate_cluster(txn_batch_config.num_eoa, txn_batch_config.num_sca);
    let txs = generate_erc20_batch(
        eoa_addresses,
        erc20_sca_address,
        txn_batch_config.num_txns_per_address,
        txn_batch_config.transaction_call_data_type,
        txn_batch_config.transaction_mode_type,
    );
    (state, bytecodes, txs)
}

/// Return a tuple of (contract_accounts, bytecodes)
pub(crate) fn generate_contract_accounts(
    num_sca: usize,
    eoa_addresses: &[Address],
) -> (Vec<(Address, PlainAccount)>, HashMap<B256, Bytecode>) {
    let mut accounts = Vec::with_capacity(num_sca);
    let mut bytecodes = HashMap::new();
    for _ in 0..num_sca {
        let gld_address = Address::new(rand::random());
        let mut gld_account =
            ERC20Token::new("Gold Token", "GLD", 18, 222_222_000_000_000_000_000_000u128)
                .add_balances(&eoa_addresses, uint!(1_000_000_000_000_000_000_U256))
                .build();
        bytecodes
            .insert(gld_account.info.code_hash, to_analysed(gld_account.info.code.take().unwrap()));
        accounts.push((gld_address, gld_account));
    }
    (accounts, bytecodes)
}
