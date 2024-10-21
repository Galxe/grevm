// Basing on https://github.com/bluealloy/revm/blob/main/bins/revme/src/cmd/statetest/runner.rs.
// These tests may seem useless:
// - They only have one transaction.
// - REVM already tests them.
// Nevertheless, they are important:
// - REVM doesn't test very tightly (not matching on expected failures, skipping tests, etc.).
// - We must use a REVM fork (for distinguishing explicit & implicit reads, etc.).
// - We use custom handlers (for lazy-updating the beneficiary account, etc.) that require "re-testing".
// - Help outline the minimal state commitment logic for pevm.

use crate::common::storage::InMemoryDB;
use alloy_chains::NamedChain;
use grevm::{GrevmError, GrevmScheduler};
use rayon::iter::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator};
use revm::db::PlainAccount;
use revm::primitives::ruint::ParseError;
use revm::primitives::{
    calc_excess_blob_gas, AccountInfo, BlobExcessGasAndPrice, BlockEnv, Bytecode, CfgEnv,
    Env as RevmEnv, TransactTo, TxEnv, KECCAK_EMPTY,
};
use revme::cmd::statetest::models::{
    Env, SpecName, TestSuite, TestUnit, TransactionParts, TxPartIndices,
};
use revme::cmd::statetest::{
    merkle_trie::{log_rlp_hash, state_merkle_trie_root},
    utils::recover_address,
};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use walkdir::{DirEntry, WalkDir};

#[path = "../common/mod.rs"]
pub mod common;

fn build_block_env(env: &Env) -> BlockEnv {
    BlockEnv {
        number: env.current_number,
        coinbase: env.current_coinbase,
        timestamp: env.current_timestamp,
        gas_limit: env.current_gas_limit,
        basefee: env.current_base_fee.unwrap_or_default(),
        difficulty: env.current_difficulty,
        prevrandao: env.current_random,
        blob_excess_gas_and_price: if let Some(current_excess_blob_gas) =
            env.current_excess_blob_gas
        {
            Some(BlobExcessGasAndPrice::new(current_excess_blob_gas.to()))
        } else if let (Some(parent_blob_gas_used), Some(parent_excess_blob_gas)) =
            (env.parent_blob_gas_used, env.parent_excess_blob_gas)
        {
            Some(BlobExcessGasAndPrice::new(calc_excess_blob_gas(
                parent_blob_gas_used.to(),
                parent_excess_blob_gas.to(),
            )))
        } else {
            None
        },
    }
}

fn build_tx_env(tx: &TransactionParts, indexes: &TxPartIndices) -> Result<TxEnv, ParseError> {
    Ok(TxEnv {
        caller: if let Some(address) = tx.sender {
            address
        } else if let Some(address) = recover_address(tx.secret_key.as_slice()) {
            address
        } else {
            panic!("Failed to parse caller") // TODO: Report test name
        },
        gas_limit: tx.gas_limit[indexes.gas].saturating_to(),
        gas_price: tx.gas_price.or(tx.max_fee_per_gas).unwrap_or_default(),
        transact_to: match tx.to {
            Some(address) => TransactTo::Call(address),
            None => TransactTo::Create,
        },
        value: tx.value[indexes.value],
        data: tx.data[indexes.data].clone(),
        nonce: Some(tx.nonce.saturating_to()),
        chain_id: Some(1), // Ethereum mainnet
        access_list: tx
            .access_lists
            .get(indexes.data)
            .and_then(Option::as_deref)
            .cloned()
            .unwrap_or_default(),
        gas_priority_fee: tx.max_priority_fee_per_gas,
        blob_hashes: tx.blob_versioned_hashes.clone(),
        max_fee_per_blob_gas: tx.max_fee_per_blob_gas,
        authorization_list: None, // TODO: Support in the upcoming hardfork
        #[cfg(feature = "optimism")]
        optimism: revm::primitives::OptimismFields::default(),
    })
}

fn run_test_unit(path: &Path, unit: TestUnit) {
    unit.post.into_par_iter().for_each(|(spec_name, tests)| {
        // Constantinople was immediately extended by Petersburg.
        // There was technically never a Constantinople transaction on mainnet
        // so REVM understandably doesn't support it (without Petersburg).
        if spec_name == SpecName::Constantinople {
            return;
        }

        tests.into_par_iter().for_each(|test| {
            let tx_env = build_tx_env(&unit.transaction, &test.indexes);
            if test.expect_exception.as_deref() == Some("TR_RLP_WRONGVALUE") && tx_env.is_err() {
                return;
            }

            let mut accounts = HashMap::new();
            let mut bytecodes = HashMap::new();
            for (address, raw_info) in unit.pre.iter() {
                let code = Bytecode::new_raw(raw_info.code.clone());
                let code_hash = if code.is_empty() {
                    KECCAK_EMPTY
                } else {
                    let code_hash = code.hash_slow();
                    bytecodes.insert(code_hash, code);
                    code_hash
                };
                let info = AccountInfo {
                    balance: raw_info.balance,
                    nonce: raw_info.nonce,
                    code_hash,
                    code: None,
                };
                accounts.insert(
                    *address,
                    PlainAccount { info, storage: raw_info.storage.clone().into_iter().collect() },
                );
            }
            let env = RevmEnv {
                cfg: CfgEnv::default().with_chain_id(NamedChain::Mainnet.into()),
                block: build_block_env(&unit.env),
                tx: Default::default(),
            };
            let db = InMemoryDB::new(accounts.clone(), bytecodes, Default::default());

            match (
                test.expect_exception.as_deref(),
                GrevmScheduler::new(spec_name.to_spec_id(), env, db.clone(), vec![tx_env.unwrap()])
                    .parallel_execute(),
            ) {
                // EIP-2681
                (Some("TransactionException.NONCE_IS_MAX"), Ok(exec_results)) => {
                    assert_eq!(exec_results.results.len(), 1);
                    // This is overly strict as we only need the newly created account's code to be empty.
                    // Extracting such account is unjustified complexity so let's live with this for now.
                    assert!(exec_results.state.state.values().all(|account| {
                        match &account.info {
                            Some(account) => account.is_empty_code_hash(),
                            None => true,
                        }
                    }));
                }
                // Remaining tests that expect execution to fail -> match error
                (Some(exception), Err(GrevmError::EvmError(error))) => {
                    println!("Error-Error: {}: {:?}", exception, error.to_string());
                    let error = error.to_string();
                    assert!(match exception {
                        "TransactionException.INSUFFICIENT_ACCOUNT_FUNDS|TransactionException.INTRINSIC_GAS_TOO_LOW" => error == "transaction validation error: call gas cost exceeds the gas limit",
                        "TransactionException.INSUFFICIENT_ACCOUNT_FUNDS" => error.contains("lack of funds"),
                        "TransactionException.INTRINSIC_GAS_TOO_LOW" => error == "transaction validation error: call gas cost exceeds the gas limit",
                        "TransactionException.SENDER_NOT_EOA" => error == "transaction validation error: reject transactions from senders with deployed code",
                        "TransactionException.PRIORITY_GREATER_THAN_MAX_FEE_PER_GAS" => error == "transaction validation error: priority fee is greater than max fee",
                        "TransactionException.GAS_ALLOWANCE_EXCEEDED" => error == "transaction validation error: caller gas limit exceeds the block gas limit",
                        "TransactionException.INSUFFICIENT_MAX_FEE_PER_GAS" => error == "transaction validation error: gas price is less than basefee",
                        "TransactionException.TYPE_3_TX_BLOB_COUNT_EXCEEDED" => error.contains("too many blobs"),
                        "TransactionException.INITCODE_SIZE_EXCEEDED" => error == "transaction validation error: create initcode size limit",
                        "TransactionException.TYPE_3_TX_ZERO_BLOBS" => error == "transaction validation error: empty blobs",
                        "TransactionException.TYPE_3_TX_CONTRACT_CREATION" => error == "transaction validation error: blob create transaction",
                        "TransactionException.TYPE_3_TX_INVALID_BLOB_VERSIONED_HASH" => error == "transaction validation error: blob version not supported",
                        "TransactionException.INSUFFICIENT_ACCOUNT_FUNDS|TransactionException.GASLIMIT_PRICE_PRODUCT_OVERFLOW" => error == "transaction validation error: overflow payment in transaction",
                        "TransactionException.TYPE_3_TX_PRE_FORK|TransactionException.TYPE_3_TX_ZERO_BLOBS" => error == "transaction validation error: blob versioned hashes not supported",
                        "TransactionException.TYPE_3_TX_PRE_FORK" => error == "transaction validation error: blob versioned hashes not supported",
                        "TransactionException.INSUFFICIENT_MAX_FEE_PER_BLOB_GAS" => error == "transaction validation error: blob gas price is greater than max fee per blob gas",
                        other => panic!("Mismatched error!\nPath: {path:?}\nExpected: {other:?}\nGot: {error:?}"),
                    });
                }
                // Tests that exepect execution to succeed -> match post state root
                (None, Ok(exec_results)) => {
                    assert_eq!(exec_results.results.len(), 1);
                    let logs = exec_results.results[0].clone().into_logs();
                    let logs_root = log_rlp_hash(&logs);
                    assert_eq!(logs_root, test.logs, "Mismatched logs root for {path:?}");

                    // This is a good reference for a minimal state/DB commitment logic for
                    // pevm/revm to meet the Ethereum specs throughout the eras.
                    for (address, bundle) in exec_results.state.state {
                        if bundle.info.is_some() {
                            let chain_state_account = accounts.entry(address).or_default();
                            for (index, slot) in bundle.storage.iter() {
                                chain_state_account.storage.insert(*index, slot.present_value);
                            }
                        }
                        if let Some(account) = bundle.info {
                            let chain_state_account = accounts.entry(address).or_default();
                            chain_state_account.info.balance = account.balance;
                            chain_state_account.info.nonce = account.nonce;
                            chain_state_account.info.code_hash = account.code_hash;
                            chain_state_account.info.code = account.code;
                        } else {
                            accounts.remove(&address);
                        }
                    }
                    // TODO: Implement our own state root calculation function to remove
                    // this conversion to [PlainAccount]
                    let plain_chain_state = accounts.into_iter().collect::<Vec<_>>();
                    state_merkle_trie_root(
                        plain_chain_state.iter().map(|(address, account)| (*address, account)),
                    );
                }
                unexpected_res => {
                    panic!("grevm doesn't match the test's expectation for {path:?}: {:?}", unexpected_res.0)
                }
            }
        });
    });
}

#[test]
fn ethereum_state_tests() {
    WalkDir::new("tests/ethereum/tests/GeneralStateTests")
        .into_iter()
        .filter_map(Result::ok)
        .map(DirEntry::into_path)
        .filter(|path| path.extension() == Some("json".as_ref()))
        // For development, we can further filter to run a small set of tests,
        // or filter out time-consuming tests like:
        //   - stTimeConsuming/**
        //   - vmPerformance/loopMul.json
        //   - stQuadraticComplexityTest/Call50000_sha256.json
        .collect::<Vec<_>>()
        .par_iter()
        .for_each(|path| {
            let raw_content = fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("Cannot read suite {path:?}: {e:?}"));
            match serde_json::from_str(&raw_content) {
                Ok(TestSuite(suite)) => {
                    suite.into_par_iter().for_each(|(_, unit)| run_test_unit(path, unit));
                }
                Err(e) => {
                    println!("Cannot parse suite {path:?}: {e:?}")
                }
            }
        });
}
