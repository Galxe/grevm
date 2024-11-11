#![allow(missing_docs)]

#[path = "../tests/common/mod.rs"]
pub mod common;

use std::sync::Arc;

use alloy_chains::NamedChain;
use alloy_rpc_types::BlockTransactions;
use common::compat;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use grevm::GrevmScheduler;
use revm::primitives::{Env, TxEnv};

fn benchmark_mainnet(c: &mut Criterion) {
    let db_latency_us = std::env::var("DB_LATENCY_US").map(|s| s.parse().unwrap()).unwrap_or(0);

    common::for_each_block_from_disk(|block, mut db| {
        db.latency_us = db_latency_us;
        let mut group = c.benchmark_group(format!(
            "Block {}({} txs, {} gas)",
            block.header.number,
            block.transactions.len(),
            block.header.gas_used
        ));

        let spec_id = compat::get_block_spec(&block.header);
        let block_env = compat::get_block_env(&block.header);
        let txs: Vec<TxEnv> = match block.transactions {
            BlockTransactions::Full(txs) => {
                txs.into_iter().map(|tx| compat::get_tx_env(tx)).collect()
            }
            _ => panic!("Missing transaction data"),
        };
        let txs = Arc::new(txs);

        let mut env = Env::default();
        env.cfg.chain_id = NamedChain::Mainnet.into();
        env.block = block_env;

        let db = Arc::new(db);

        group.bench_function("Origin Sequential", |b| {
            b.iter(|| {
                common::execute_revm_sequential(
                    black_box(db.clone()),
                    black_box(spec_id),
                    black_box(env.clone()),
                    black_box(&*txs),
                )
            })
        });

        group.bench_function("Grevm Parallel", |b| {
            b.iter(|| {
                let mut executor = GrevmScheduler::new(
                    black_box(spec_id),
                    black_box(env.clone()),
                    black_box(db.clone()),
                    black_box(txs.clone()),
                    None,
                );
                executor.parallel_execute()
            })
        });

        group.bench_function("Grevm Sequential", |b| {
            b.iter(|| {
                let mut executor = GrevmScheduler::new(
                    black_box(spec_id),
                    black_box(env.clone()),
                    black_box(db.clone()),
                    black_box(txs.clone()),
                    None,
                );
                executor.force_sequential_execute()
            })
        });
    });
}

criterion_group!(benches, benchmark_mainnet);
criterion_main!(benches);
