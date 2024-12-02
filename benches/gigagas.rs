#![allow(missing_docs)]

#[path = "../tests/common/mod.rs"]
pub mod common;

#[path = "../tests/erc20/mod.rs"]
pub mod erc20;

#[path = "../tests/uniswap/mod.rs"]
pub mod uniswap;

use crate::{erc20::erc20_contract::ERC20Token, uniswap::contract::SingleSwap};
use alloy_chains::NamedChain;
use common::storage::InMemoryDB;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use fastrace::{collector::Config, prelude::*};
use fastrace_jaeger::JaegerReporter;
use grevm::GrevmScheduler;
use metrics::{SharedString, Unit};
use metrics_util::{
    debugging::{DebugValue, DebuggingRecorder},
    CompositeKey, MetricKind,
};
use rand::Rng;
use revm::primitives::{alloy_primitives::U160, Address, Env, SpecId, TransactTo, TxEnv, U256};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

const GIGA_GAS: u64 = 1_000_000_000;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn get_metrics_counter_value(
    snapshot: &HashMap<CompositeKey, (Option<Unit>, Option<SharedString>, DebugValue)>,
    name: &'static str,
) -> u64 {
    match snapshot
        .get(&CompositeKey::new(MetricKind::Histogram, metrics::Key::from_static_name(name)))
    {
        Some((_, _, DebugValue::Histogram(value))) => {
            value.last().cloned().map_or(0, |ov| ov.0 as u64)
        }
        _ => panic!("{:?} not found", name),
    }
}

fn bench(c: &mut Criterion, name: &str, db: InMemoryDB, txs: Vec<TxEnv>) {
    let mut env = Env::default();
    env.cfg.chain_id = NamedChain::Mainnet.into();
    env.block.coinbase = Address::from(U160::from(common::MINER_ADDRESS));
    let db = Arc::new(db);
    let txs = Arc::new(txs);

    let mut group = c.benchmark_group(format!("{}({} txs)", name, txs.len()));
    let mut num_iter: usize = 0;
    let mut execution_time_ns: u64 = 0;
    group.bench_function("Grevm Parallel", |b| {
        b.iter(|| {
            num_iter += 1;
            let recorder = DebuggingRecorder::new();
            let root = Span::root(format!("{name} Grevm Parallel"), SpanContext::random());
            let _guard = root.set_local_parent();
            metrics::with_local_recorder(&recorder, || {
                let mut executor = GrevmScheduler::new(
                    black_box(SpecId::LATEST),
                    black_box(env.clone()),
                    black_box(db.clone()),
                    black_box(txs.clone()),
                    None,
                );
                let _ = executor.parallel_execute();

                let snapshot = recorder.snapshotter().snapshot().into_hashmap();
                execution_time_ns +=
                    get_metrics_counter_value(&snapshot, "grevm.parallel_execute_time");
                execution_time_ns += get_metrics_counter_value(&snapshot, "grevm.validate_time");
            });
        })
    });
    println!(
        "{} Grevm Parallel average execution time: {:.2} ms",
        name,
        execution_time_ns as f64 / num_iter as f64 / 1000000.0
    );

    let mut num_iter: usize = 0;
    let mut execution_time_ns: u64 = 0;
    group.bench_function("Grevm Sequential", |b| {
        b.iter(|| {
            num_iter += 1;
            let recorder = DebuggingRecorder::new();
            let root = Span::root(format!("{name} Grevm Sequential"), SpanContext::random());
            let _guard = root.set_local_parent();
            metrics::with_local_recorder(&recorder, || {
                let mut executor = GrevmScheduler::new(
                    black_box(SpecId::LATEST),
                    black_box(env.clone()),
                    black_box(db.clone()),
                    black_box(txs.clone()),
                    None,
                );
                let _ = executor.force_sequential_execute();

                let snapshot = recorder.snapshotter().snapshot().into_hashmap();
                execution_time_ns +=
                    get_metrics_counter_value(&snapshot, "grevm.sequential_execute_time");
            });
        })
    });
    println!(
        "{} Grevm Sequential average execution time: {:.2} ms",
        name,
        execution_time_ns as f64 / num_iter as f64 / 1000000.0
    );

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

fn pick_account_idx(num_eoa: usize, hot_ratio: f64) -> usize {
    if hot_ratio <= 0.0 {
        // Uniform workload
        return rand::random::<usize>() % num_eoa;
    }

    // Let `hot_ratio` of transactions conducted by 10% of hot accounts
    let hot_start_idx = (num_eoa as f64 * 0.9) as usize;
    if rand::thread_rng().gen_range(0.0..1.0) < hot_ratio {
        // Access hot
        hot_start_idx + rand::random::<usize>() % (num_eoa - hot_start_idx)
    } else {
        rand::random::<usize>() % hot_start_idx
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

    bench(
        c,
        "Dependent Raw Transfers",
        db,
        (0..block_size)
            .map(|_| {
                let from = Address::from(U160::from(
                    common::START_ADDRESS + pick_account_idx(num_eoa, hot_ratio),
                ));
                let to = Address::from(U160::from(
                    common::START_ADDRESS + pick_account_idx(num_eoa, hot_ratio),
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
    let filter: String = std::env::var("FILTER").unwrap_or_default();
    let filter: HashSet<&str> = filter.split(',').filter(|s| !s.is_empty()).collect();

    if filter.is_empty() || filter.contains("raw_transfers") {
        bench_raw_transfers(c, db_latency_us);
    }
    if filter.is_empty() || filter.contains("dependent_raw_transfers") {
        bench_dependent_raw_transfers(c, db_latency_us, num_eoa, hot_ratio);
    }
    if filter.is_empty() || filter.contains("erc20") {
        bench_erc20(c, db_latency_us);
    }
    if filter.is_empty() || filter.contains("dependent_erc20") {
        bench_dependent_erc20(c, db_latency_us, num_eoa, hot_ratio);
    }
    if filter.is_empty() || filter.contains("uniswap") {
        bench_uniswap(c, db_latency_us);
    }
    if filter.is_empty() || filter.contains("hybrid") {
        bench_hybrid(c, db_latency_us, num_eoa, hot_ratio);
    }

    fastrace::flush();
}

fn bench_erc20(c: &mut Criterion, db_latency_us: u64) {
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

fn bench_dependent_erc20(c: &mut Criterion, db_latency_us: u64, num_eoa: usize, hot_ratio: f64) {
    let block_size = (GIGA_GAS as f64 / erc20::ESTIMATED_GAS_USED as f64).ceil() as usize;
    let (mut state, bytecodes, eoa, sca) = erc20::generate_cluster(num_eoa, 1);
    let miner = common::mock_miner_account();
    state.insert(miner.0, miner.1);
    let mut txs = Vec::with_capacity(block_size);
    let sca = sca[0];

    for _ in 0..block_size {
        let from = eoa[pick_account_idx(num_eoa, hot_ratio)];
        let to = eoa[pick_account_idx(num_eoa, hot_ratio)];
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

    let mut state = common::mock_block_accounts(common::START_ADDRESS, num_eoa);
    let eoa_addresses = state.keys().cloned().collect::<Vec<_>>();
    let mut txs = Vec::with_capacity(num_native_transfer + num_erc20_transfer + num_uniswap);

    for _ in 0..num_native_transfer {
        let from =
            Address::from(U160::from(common::START_ADDRESS + pick_account_idx(num_eoa, hot_ratio)));
        let to =
            Address::from(U160::from(common::START_ADDRESS + pick_account_idx(num_eoa, hot_ratio)));
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
                common::START_ADDRESS + pick_account_idx(num_eoa, hot_ratio),
            ));
            let to = Address::from(U160::from(
                common::START_ADDRESS + pick_account_idx(num_eoa, hot_ratio),
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
                    common::START_ADDRESS + pick_account_idx(num_eoa, hot_ratio),
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
