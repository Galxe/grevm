#![allow(missing_docs)]

#[path = "../tests/common/mod.rs"]
pub mod common;

#[path = "../tests/erc20/mod.rs"]
pub mod erc20;

#[path = "../tests/uniswap/mod.rs"]
pub mod uniswap;

use crate::erc20::erc20_contract::ERC20Token;
use crate::uniswap::contract::SingleSwap;
use alloy_chains::NamedChain;
use common::storage::InMemoryDB;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use fastrace::collector::Config;
use fastrace::prelude::*;
use fastrace_jaeger::JaegerReporter;
use grevm::GrevmScheduler;
use rand::Rng;
use revm::primitives::alloy_primitives::U160;
use revm::primitives::{Address, Env, SpecId, TransactTo, TxEnv, U256};
use std::collections::HashMap;
use std::sync::Arc;

const GIGA_GAS: u64 = 1_000_000_000;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn bench(c: &mut Criterion, name: &str, db: InMemoryDB, txs: Vec<TxEnv>) {
    let mut env = Env::default();
    env.cfg.chain_id = NamedChain::Mainnet.into();
    env.block.coinbase = Address::from(U160::from(common::MINER_ADDRESS));
    let db = Arc::new(db);

    let mut group = c.benchmark_group(name);
    group.bench_function("Origin Sequential", |b| {
        b.iter(|| {
            common::execute_revm_sequential(
                black_box(db.clone()),
                black_box(SpecId::LATEST),
                black_box(env.clone()),
                black_box(txs.clone()),
            )
        })
    });
    group.bench_function("Grevm Parallel", |b| {
        b.iter(|| {
            let root = Span::root(format!("{name} Grevm Parallel"), SpanContext::random());
            let _guard = root.set_local_parent();
            let executor = GrevmScheduler::new(
                black_box(SpecId::LATEST),
                black_box(env.clone()),
                black_box(db.clone()),
                black_box(txs.clone()),
            );
            executor.parallel_execute()
        })
    });
    group.bench_function("Grevm Sequential", |b| {
        b.iter(|| {
            let root = Span::root(format!("{name} Grevm Sequential"), SpanContext::random());
            let _guard = root.set_local_parent();
            let executor = GrevmScheduler::new(
                black_box(SpecId::LATEST),
                black_box(env.clone()),
                black_box(db.clone()),
                black_box(txs.clone()),
            );
            executor.force_sequential_execute()
        })
    });
    group.finish();
}

fn bench_raw_transfers(c: &mut Criterion, db_latency_us: u64) {
    let block_size = (GIGA_GAS as f64 / common::TRANSFER_GAS_LIMIT as f64).ceil() as usize;
    let accounts = common::mock_block_accounts(common::START_ADDRESS, block_size);
    let mut db = InMemoryDB::new(accounts, Default::default(), Default::default());
    db.latency_us = db_latency_us;
    bench(
        c,
        "Independent Raw Transfers",
        db,
        (0..block_size)
            .map(|i| {
                let address = Address::from(U160::from(common::START_ADDRESS + i));
                TxEnv {
                    caller: address,
                    transact_to: TransactTo::Call(address),
                    value: U256::from(1),
                    gas_limit: common::TRANSFER_GAS_LIMIT,
                    gas_price: U256::from(1),
                    ..TxEnv::default()
                }
            })
            .collect::<Vec<_>>(),
    );
}

fn get_account_idx(num_eoa: usize, hot_start_idx: usize, hot_ratio: f64) -> usize {
    if hot_ratio <= 0.0 {
        // Uniform workload
        rand::random::<usize>() % num_eoa
    } else if rand::thread_rng().gen_range(0.0..1.0) < hot_ratio {
        // Access hot
        hot_start_idx + rand::random::<usize>() % (num_eoa - hot_start_idx)
    } else {
        rand::random::<usize>() % (num_eoa - hot_start_idx)
    }
}

fn bench_dependent_raw_transfers(
    c: &mut Criterion,
    db_latency_us: u64,
    num_eoa: usize,
    hot_ratio: f64,
) {
    let block_size = (GIGA_GAS as f64 / common::TRANSFER_GAS_LIMIT as f64).ceil() as usize;
    let accounts = common::mock_block_accounts(common::START_ADDRESS, num_eoa);
    let mut db = InMemoryDB::new(accounts, Default::default(), Default::default());
    db.latency_us = db_latency_us;

    // Let 10% of the accounts be hot accounts
    let hot_start_idx = common::START_ADDRESS + (num_eoa as f64 * 0.9) as usize;

    bench(
        c,
        "Dependent Raw Transfers",
        db,
        (0..block_size)
            .map(|_| {
                let from = Address::from(U160::from(
                    common::START_ADDRESS + get_account_idx(num_eoa, hot_start_idx, hot_ratio),
                ));
                let to = Address::from(U160::from(
                    common::START_ADDRESS + get_account_idx(num_eoa, hot_start_idx, hot_ratio),
                ));
                TxEnv {
                    caller: from,
                    transact_to: TransactTo::Call(to),
                    value: U256::from(1),
                    gas_limit: common::TRANSFER_GAS_LIMIT,
                    gas_price: U256::from(1),
                    ..TxEnv::default()
                }
            })
            .collect::<Vec<_>>(),
    );
}

fn benchmark_gigagas(c: &mut Criterion) {
    let reporter = JaegerReporter::new("127.0.0.1:6831".parse().unwrap(), "gigagas").unwrap();
    fastrace::set_reporter(reporter, Config::default());

    // TODO(gravity): Create options from toml file if there are more
    let db_latency_us = std::env::var("DB_LATENCY_US").map(|s| s.parse().unwrap()).unwrap_or(0);
    let num_eoa = std::env::var("NUM_EOA").map(|s| s.parse().unwrap()).unwrap_or(0);
    let hot_ratio = std::env::var("HOT_RATIO").map(|s| s.parse().unwrap()).unwrap_or(0.0);
    bench_raw_transfers(c, db_latency_us);
    bench_dependent_raw_transfers(c, db_latency_us, num_eoa, hot_ratio);
    benchmark_erc20(c, db_latency_us);
    benchmark_dependent_erc20(c, db_latency_us, num_eoa, hot_ratio);
    bench_uniswap(c, db_latency_us);
    bench_hybrid(c, db_latency_us, num_eoa, hot_ratio);

    fastrace::flush();
}

fn benchmark_erc20(c: &mut Criterion, db_latency_us: u64) {
    let block_size = (GIGA_GAS as f64 / erc20::ESTIMATED_GAS_USED as f64).ceil() as usize;
    let (mut state, bytecodes, eoa, sca) = erc20::generate_cluster(block_size, 1);
    let miner = common::mock_miner_account();
    state.insert(miner.0, miner.1);
    let mut txs = Vec::with_capacity(block_size);
    let sca = sca[0];
    for addr in eoa {
        let tx = TxEnv {
            caller: addr,
            transact_to: TransactTo::Call(sca),
            value: U256::from(0),
            gas_limit: erc20::GAS_LIMIT,
            gas_price: U256::from(1),
            data: ERC20Token::transfer(addr, U256::from(900)),
            ..TxEnv::default()
        };
        txs.push(tx);
    }
    let mut db = InMemoryDB::new(state, bytecodes, Default::default());
    db.latency_us = db_latency_us;

    bench(c, "Independent ERC20", db, txs);
}

fn benchmark_dependent_erc20(
    c: &mut Criterion,
    db_latency_us: u64,
    num_eoa: usize,
    hot_ratio: f64,
) {
    let block_size = (GIGA_GAS as f64 / erc20::ESTIMATED_GAS_USED as f64).ceil() as usize;
    let (mut state, bytecodes, eoa, sca) = erc20::generate_cluster(num_eoa, 1);
    let miner = common::mock_miner_account();
    state.insert(miner.0, miner.1);
    let mut txs = Vec::with_capacity(block_size);
    let sca = sca[0];

    // Let 10% of the accounts be hot accounts
    let hot_start_idx = common::START_ADDRESS + (num_eoa as f64 * 0.9) as usize;

    for _ in 0..block_size {
        let from = eoa[get_account_idx(num_eoa, hot_start_idx, hot_ratio)];
        let to = eoa[get_account_idx(num_eoa, hot_start_idx, hot_ratio)];
        let tx = TxEnv {
            caller: from,
            transact_to: TransactTo::Call(sca),
            value: U256::from(0),
            gas_limit: erc20::GAS_LIMIT,
            gas_price: U256::from(1),
            data: ERC20Token::transfer(to, U256::from(900)),
            ..TxEnv::default()
        };
        txs.push(tx);
    }
    let mut db = InMemoryDB::new(state, bytecodes, Default::default());
    db.latency_us = db_latency_us;

    bench(c, "Dependent ERC20", db, txs);
}

fn bench_uniswap(c: &mut Criterion, db_latency_us: u64) {
    let block_size = (GIGA_GAS as f64 / uniswap::ESTIMATED_GAS_USED as f64).ceil() as usize;
    let mut final_state = HashMap::from([common::mock_miner_account()]);
    let mut final_bytecodes = HashMap::default();
    let mut final_txs = Vec::<TxEnv>::new();
    for _ in 0..block_size {
        let (state, bytecodes, txs) = uniswap::generate_cluster(1, 1);
        final_state.extend(state);
        final_bytecodes.extend(bytecodes);
        final_txs.extend(txs);
    }
    let mut db = InMemoryDB::new(final_state, final_bytecodes, Default::default());
    db.latency_us = db_latency_us;
    bench(c, "Independent Uniswap", db, final_txs);
}

fn bench_hybrid(c: &mut Criterion, db_latency_us: u64, num_eoa: usize, hot_ratio: f64) {
    // 60% native transfer, 20% erc20 transfer, 20% uniswap
    let num_native_transfer =
        (GIGA_GAS as f64 * 0.6 / common::TRANSFER_GAS_LIMIT as f64).ceil() as usize;
    let num_erc20_transfer =
        (GIGA_GAS as f64 * 0.2 / erc20::ESTIMATED_GAS_USED as f64).ceil() as usize;
    let num_uniswap = (GIGA_GAS as f64 * 0.2 / uniswap::ESTIMATED_GAS_USED as f64).ceil() as usize;

    // Let 10% of the accounts be hot accounts
    let hot_start_idx = common::START_ADDRESS + (num_eoa as f64 * 0.9) as usize;
    let mut state = common::mock_block_accounts(common::START_ADDRESS, num_eoa);
    let eoa_addresses = state.keys().cloned().collect::<Vec<_>>();
    let mut txs = Vec::with_capacity(num_native_transfer + num_erc20_transfer + num_uniswap);

    for _ in 0..num_native_transfer {
        let from = Address::from(U160::from(
            common::START_ADDRESS + get_account_idx(num_eoa, hot_start_idx, hot_ratio),
        ));
        let to = Address::from(U160::from(
            common::START_ADDRESS + get_account_idx(num_eoa, hot_start_idx, hot_ratio),
        ));
        let tx = TxEnv {
            caller: from,
            transact_to: TransactTo::Call(to),
            value: U256::from(1),
            gas_limit: common::TRANSFER_GAS_LIMIT,
            gas_price: U256::from(1),
            ..TxEnv::default()
        };
        txs.push(tx);
    }

    const NUM_ERC20_SCA: usize = 3;
    let (erc20_contract_accounts, erc20_bytecodes) =
        erc20::generate_contract_accounts(NUM_ERC20_SCA, &eoa_addresses);
    for (sca_addr, _) in erc20_contract_accounts.iter() {
        for _ in 0..(num_erc20_transfer / NUM_ERC20_SCA) {
            let from = Address::from(U160::from(
                common::START_ADDRESS + get_account_idx(num_eoa, hot_start_idx, hot_ratio),
            ));
            let to = Address::from(U160::from(
                common::START_ADDRESS + get_account_idx(num_eoa, hot_start_idx, hot_ratio),
            ));
            let tx = TxEnv {
                caller: from,
                transact_to: TransactTo::Call(*sca_addr),
                value: U256::from(0),
                gas_limit: erc20::GAS_LIMIT,
                gas_price: U256::from(1),
                data: ERC20Token::transfer(to, U256::from(900)),
                ..TxEnv::default()
            };
            txs.push(tx);
        }
    }
    state.extend(erc20_contract_accounts.into_iter());

    let mut bytecodes = erc20_bytecodes;
    const NUM_UNISWAP_CLUSTER: usize = 2;
    for _ in 0..NUM_UNISWAP_CLUSTER {
        let (uniswap_contract_accounts, uniswap_bytecodes, single_swap_address) =
            uniswap::generate_contract_accounts(&eoa_addresses);
        state.extend(uniswap_contract_accounts);
        bytecodes.extend(uniswap_bytecodes);
        for _ in 0..(num_uniswap / NUM_UNISWAP_CLUSTER) {
            let data_bytes = if rand::random::<u64>() % 2 == 0 {
                SingleSwap::sell_token0(U256::from(2000))
            } else {
                SingleSwap::sell_token1(U256::from(2000))
            };

            txs.push(TxEnv {
                caller: Address::from(U160::from(
                    common::START_ADDRESS + get_account_idx(num_eoa, hot_start_idx, hot_ratio),
                )),
                gas_limit: uniswap::GAS_LIMIT,
                gas_price: U256::from(0xb2d05e07u64),
                transact_to: TransactTo::Call(single_swap_address),
                data: data_bytes,
                ..TxEnv::default()
            })
        }
    }

    let mut db = InMemoryDB::new(state, bytecodes, Default::default());
    db.latency_us = db_latency_us;

    bench(c, "Hybrid", db, txs);
}

criterion_group!(benches, benchmark_gigagas);
criterion_main!(benches);
