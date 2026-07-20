use revm_database::PlainAccount;
use revm_primitives::{
    Address, HashMap, KECCAK_EMPTY, U256, address, alloy_primitives::U160, uint,
};
use revm_state::AccountInfo;

pub const MINER_ADDRESS: Address = address!("00000000000000000000000000000000000000ff");

pub fn mock_miner_account() -> (Address, PlainAccount) {
    let account = PlainAccount {
        info: AccountInfo { balance: U256::from(0), nonce: 1, code_hash: KECCAK_EMPTY, code: None },
        storage: Default::default(),
    };
    (MINER_ADDRESS, account)
}

pub fn mock_eoa_address(idx: usize) -> Address {
    // skip precompile address
    const ADDRESS_OFFSET: usize = 1000;
    Address::from(U160::from(idx + ADDRESS_OFFSET))
}

pub fn mock_eoa_account(idx: usize) -> (Address, PlainAccount) {
    let address = mock_eoa_address(idx);
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

pub fn mock_block_accounts(size: usize) -> HashMap<Address, PlainAccount> {
    let mut accounts: HashMap<Address, PlainAccount> = (0..size).map(mock_eoa_account).collect();
    let miner = mock_miner_account();
    accounts.insert(miner.0, miner.1);
    accounts
}
