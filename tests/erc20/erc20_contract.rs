use crate::common::storage::{from_address, from_indices, from_short_string, StorageBuilder};
use lazy_static::lazy_static;
use revm::db::PlainAccount;
use revm::interpreter::analysis::to_analysed;
use revm::primitives::hex::FromHex;
use revm::primitives::ruint::UintTryFrom;
use revm::primitives::{fixed_bytes, AccountInfo, Address, Bytecode, Bytes, B256, U256};
use std::collections::HashMap;

const ERC20_TOKEN: &str = include_str!("./contracts/ERC20Token.hex");

// $ forge inspect ERC20Token methods

lazy_static! {
    pub static ref ERC20_ALLLOWANCE: U256 = U256::from(0xdd62ed3e_i64);
    pub static ref ERC20_APPROVE: U256 = U256::from(0x095ea7b3_i64);
    pub static ref ERC20_BALANCE_OF: U256 = U256::from(0x70a08231_i64);
    pub static ref ERC20_DECIMALS: U256 = U256::from(0x313ce567_i64);
    pub static ref ERC20_DECREASE_ALLOWANCE: U256 = U256::from(0xa457c2d7_i64);
    pub static ref ERC20_INCREASE_ALLOWANCE: U256 = U256::from(0x39509351_i64);
    pub static ref ERC20_NAME: U256 = U256::from(0x06fdde03_i64);
    pub static ref ERC20_SYMBOL: U256 = U256::from(0x95d89b41_i64);
    pub static ref ERC20_TOTAL_SUPPLY: U256 = U256::from(0x18160ddd_i64);
    pub static ref ERC20_TRANSFER: U256 = U256::from(0xa9059cbb_i64);
    pub static ref ERC20_TRANSFER_FROM: U256 = U256::from(0x23b872dd_i64);
}

// @risechain/op-test-bench/foundry/src/ERC20Token.sol
#[derive(Debug, Default)]
pub struct ERC20Token {
    name: String,
    symbol: String,
    decimals: U256,
    initial_supply: U256,
    balances: HashMap<Address, U256>,
    allowances: HashMap<(Address, Address), U256>,
}

impl ERC20Token {
    pub fn new<U, V>(name: &str, symbol: &str, decimals: U, initial_supply: V) -> Self
    where
        U256: UintTryFrom<U>,
        U256: UintTryFrom<V>,
    {
        Self {
            name: String::from(name),
            symbol: String::from(symbol),
            decimals: U256::from(decimals),
            initial_supply: U256::from(initial_supply),
            balances: HashMap::new(),
            allowances: HashMap::new(),
        }
    }

    pub fn add_balances(&mut self, addresses: &[Address], amount: U256) -> &mut Self {
        for address in addresses {
            self.balances.insert(*address, amount);
        }
        self
    }

    pub fn add_allowances(
        &mut self,
        addresses: &[Address],
        spender: Address,
        amount: U256,
    ) -> &mut Self {
        for address in addresses {
            self.allowances.insert((*address, spender), amount);
        }
        self
    }

    // | Name         | Type                                            | Slot | Offset | Bytes |
    // |--------------|-------------------------------------------------|------|--------|-------|
    // | _balances    | mapping(address => uint256)                     | 0    | 0      | 32    |
    // | _allowances  | mapping(address => mapping(address => uint256)) | 1    | 0      | 32    |
    // | _totalSupply | uint256                                         | 2    | 0      | 32    |
    // | _name        | string                                          | 3    | 0      | 32    |
    // | _symbol      | string                                          | 4    | 0      | 32    |
    // | _decimals    | uint8                                           | 5    | 0      | 1     |
    pub fn build(&self) -> PlainAccount {
        let hex = ERC20_TOKEN.trim();
        let bytecode = Bytecode::new_raw(Bytes::from_hex(hex).unwrap());
        let bytecode = to_analysed(bytecode);

        let mut store = StorageBuilder::new();
        store.set(0, 0); // mapping
        store.set(1, 0); // mapping
        store.set(2, self.initial_supply);
        store.set(3, from_short_string(&self.name));
        store.set(4, from_short_string(&self.symbol));
        store.set(5, self.decimals);

        for (address, amount) in self.balances.iter() {
            store.set(from_indices(0, &[from_address(*address)]), *amount);
        }

        for ((address, spender), amount) in self.allowances.iter() {
            store.set(from_indices(1, &[from_address(*address), from_address(*spender)]), *amount);
        }

        PlainAccount {
            info: AccountInfo {
                balance: U256::ZERO,
                nonce: 1,
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
            },
            storage: store.build(),
        }
    }

    pub fn transfer(recipient: Address, amount: U256) -> Bytes {
        Bytes::from(
            [
                &fixed_bytes!("a9059cbb")[..],
                // ERC20_TRANSFER.to(),
                &B256::from(from_address(recipient))[..],
                &B256::from(amount)[..],
            ]
            .concat(),
        )
    }

    pub fn transfer_from(sender: Address, recipient: Address, amount: U256) -> Bytes {
        Bytes::from(
            [
                &fixed_bytes!("23b872dd")[..],
                &B256::from(from_address(sender))[..],
                &B256::from(from_address(recipient))[..],
                &B256::from(amount)[..],
            ]
            .concat(),
        )
    }
}
