pub mod contract;

use crate::erc20::erc20_contract::ERC20Token;
use contract::{SingleSwap, SwapRouter, UniswapV3Factory, UniswapV3Pool, WETH9};
use revm::db::PlainAccount;
use revm::interpreter::analysis::to_analysed;
use revm::primitives::{
    uint, AccountInfo, Address, Bytecode, Bytes, HashMap, TransactTo, TxEnv, B256, U256,
};

pub const GAS_LIMIT: u64 = 200_000;
pub const ESTIMATED_GAS_USED: u64 = 155_934;

/// Return a tuple of (contract_accounts, bytecodes, single_swap_address)
/// `single_swap_address` is the entrance of the contract.
pub(crate) fn generate_contract_accounts(
    eoa_addresses: &[Address],
) -> (Vec<(Address, PlainAccount)>, HashMap<B256, Bytecode>, Address) {
    let (dai_address, usdc_address) = {
        let x = Address::new(rand::random());
        let y = Address::new(rand::random());
        (std::cmp::min(x, y), std::cmp::max(x, y))
    };
    let pool_init_code_hash = B256::new(rand::random());
    let swap_router_address = Address::new(rand::random());
    let single_swap_address = Address::new(rand::random());
    let weth9_address = Address::new(rand::random());
    let owner = Address::new(rand::random());
    let factory_address = Address::new(rand::random());
    let nonfungible_position_manager_address = Address::new(rand::random());
    let pool_address = UniswapV3Pool::new(dai_address, usdc_address, factory_address)
        .get_address(factory_address, pool_init_code_hash);

    let weth9_account = WETH9::new().build();

    let dai_account = ERC20Token::new("DAI", "DAI", 18, 222_222_000_000_000_000_000_000u128)
        .add_balances(&[pool_address], uint!(111_111_000_000_000_000_000_000_U256))
        .add_balances(&eoa_addresses, uint!(1_000_000_000_000_000_000_U256))
        .add_allowances(&eoa_addresses, single_swap_address, uint!(1_000_000_000_000_000_000_U256))
        .build();

    let usdc_account = ERC20Token::new("USDC", "USDC", 18, 222_222_000_000_000_000_000_000u128)
        .add_balances(&[pool_address], uint!(111_111_000_000_000_000_000_000_U256))
        .add_balances(&eoa_addresses, uint!(1_000_000_000_000_000_000_U256))
        .add_allowances(&eoa_addresses, single_swap_address, uint!(1_000_000_000_000_000_000_U256))
        .build();

    let factory_account = UniswapV3Factory::new(owner)
        .add_pool(dai_address, usdc_address, pool_address)
        .build(factory_address);

    let pool_account = UniswapV3Pool::new(dai_address, usdc_address, factory_address)
        .add_position(
            nonfungible_position_manager_address,
            -600000,
            600000,
            [
                uint!(0x00000000000000000000000000000000000000000000178756e190b388651605_U256),
                uint!(0x0000000000000000000000000000000000000000000000000000000000000000_U256),
                uint!(0x0000000000000000000000000000000000000000000000000000000000000000_U256),
                uint!(0x0000000000000000000000000000000000000000000000000000000000000000_U256),
            ],
        )
        .add_tick(
            -600000,
            [
                uint!(0x000000000000178756e190b388651605000000000000178756e190b388651605_U256),
                uint!(0x0000000000000000000000000000000000000000000000000000000000000000_U256),
                uint!(0x0000000000000000000000000000000000000000000000000000000000000000_U256),
                uint!(0x0100000001000000000000000000000000000000000000000000000000000000_U256),
            ],
        )
        .add_tick(
            600000,
            [
                uint!(0xffffffffffffe878a91e6f4c779ae9fb000000000000178756e190b388651605_U256),
                uint!(0x0000000000000000000000000000000000000000000000000000000000000000_U256),
                uint!(0x0000000000000000000000000000000000000000000000000000000000000000_U256),
                uint!(0x0100000000000000000000000000000000000000000000000000000000000000_U256),
            ],
        )
        .build(pool_address);

    let swap_router_account =
        SwapRouter::new(weth9_address, factory_address, pool_init_code_hash).build();

    let single_swap_account =
        SingleSwap::new(swap_router_address, dai_address, usdc_address).build();

    let mut accounts = Vec::new();
    accounts.push((weth9_address, weth9_account));
    accounts.push((dai_address, dai_account));
    accounts.push((usdc_address, usdc_account));
    accounts.push((factory_address, factory_account));
    accounts.push((pool_address, pool_account));
    accounts.push((swap_router_address, swap_router_account));
    accounts.push((single_swap_address, single_swap_account));

    let mut bytecodes = HashMap::new();
    for (_, account) in accounts.iter_mut() {
        let code = account.info.code.take();
        if let Some(code) = code {
            let code = to_analysed(code);
            bytecodes.insert(account.info.code_hash, code);
        }
    }

    (accounts, bytecodes, single_swap_address)
}

pub fn generate_cluster(
    num_people: usize,
    num_swaps_per_person: usize,
) -> (HashMap<Address, PlainAccount>, HashMap<B256, Bytecode>, Vec<TxEnv>) {
    // TODO: Better randomness control. Sometimes we want duplicates to test
    // dependent transactions, sometimes we want to guarantee non-duplicates
    // for independent benchmarks.
    let people_addresses: Vec<Address> =
        (0..num_people).map(|_| Address::new(rand::random())).collect();

    let (contract_accounts, bytecodes, single_swap_address) =
        generate_contract_accounts(&people_addresses);
    let mut state: HashMap<Address, PlainAccount> = contract_accounts.into_iter().collect();

    for person in people_addresses.iter() {
        state.insert(
            *person,
            PlainAccount {
                info: AccountInfo {
                    balance: uint!(4_567_000_000_000_000_000_000_U256),
                    ..AccountInfo::default()
                },
                ..PlainAccount::default()
            },
        );
    }

    let mut txs = Vec::new();

    for nonce in 0..num_swaps_per_person {
        for person in people_addresses.iter() {
            let data_bytes = match nonce % 4 {
                0 => SingleSwap::sell_token0(U256::from(2000)),
                1 => SingleSwap::sell_token1(U256::from(2000)),
                2 => SingleSwap::buy_token0(U256::from(1000), U256::from(2000)),
                3 => SingleSwap::buy_token1(U256::from(1000), U256::from(2000)),
                _ => Default::default(),
            };

            txs.push(TxEnv {
                caller: *person,
                gas_limit: GAS_LIMIT,
                gas_price: U256::from(0xb2d05e07u64),
                transact_to: TransactTo::Call(single_swap_address),
                data: Bytes::from(data_bytes),
                nonce: Some(nonce as u64),
                ..TxEnv::default()
            })
        }
    }

    (state, bytecodes, txs)
}
