use alloy_consensus::TxType;
use alloy_rpc_types::{Header, Transaction};
use revm::primitives::{BlobExcessGasAndPrice, BlockEnv, SpecId, TransactTo, TxEnv, U256};

pub(crate) fn get_block_spec(header: &Header) -> SpecId {
    let number = header.number;
    let total_difficulty = header.total_difficulty.unwrap();

    if header.timestamp >= 1710338135 {
        SpecId::CANCUN
    } else if header.timestamp >= 1681338455 {
        SpecId::SHANGHAI
    } else if total_difficulty.saturating_sub(header.difficulty)
        >= U256::from(58_750_000_000_000_000_000_000_u128)
    {
        SpecId::MERGE
    } else if number >= 12965000 {
        SpecId::LONDON
    } else if number >= 12244000 {
        SpecId::BERLIN
    } else if number >= 9069000 {
        SpecId::ISTANBUL
    } else if number >= 7280000 {
        SpecId::PETERSBURG
    } else if number >= 4370000 {
        SpecId::BYZANTIUM
    } else if number >= 2675000 {
        SpecId::SPURIOUS_DRAGON
    } else if number >= 2463000 {
        SpecId::TANGERINE
    } else if number >= 1150000 {
        SpecId::HOMESTEAD
    } else {
        SpecId::FRONTIER
    }
}

pub(crate) fn get_block_env(header: &Header) -> BlockEnv {
    BlockEnv {
        number: U256::from(header.number),
        coinbase: header.miner,
        timestamp: U256::from(header.timestamp),
        gas_limit: U256::from(header.gas_limit),
        basefee: U256::from(header.base_fee_per_gas.unwrap_or_default()),
        difficulty: header.difficulty,
        prevrandao: header.mix_hash,
        blob_excess_gas_and_price: header
            .excess_blob_gas
            .map(|excess_blob_gas| BlobExcessGasAndPrice::new(excess_blob_gas as u64)),
    }
}

pub(crate) fn get_gas_price(tx: &Transaction) -> U256 {
    let tx_type_raw: u8 = tx.transaction_type.unwrap_or_default();
    let tx_type = TxType::try_from(tx_type_raw).unwrap();
    match tx_type {
        TxType::Legacy | TxType::Eip2930 => tx.gas_price.map(U256::from).unwrap(),
        TxType::Eip1559 | TxType::Eip4844 | TxType::Eip7702 => {
            tx.max_fee_per_gas.map(U256::from).unwrap()
        }
    }
}

pub(crate) fn get_tx_env(tx: Transaction) -> TxEnv {
    TxEnv {
        caller: tx.from,
        gas_limit: tx.gas.try_into().unwrap(),
        gas_price: get_gas_price(&tx),
        gas_priority_fee: tx.max_priority_fee_per_gas.map(U256::from),
        transact_to: match tx.to {
            Some(address) => TransactTo::Call(address),
            None => TransactTo::Create,
        },
        value: tx.value,
        data: tx.input,
        nonce: Some(tx.nonce),
        chain_id: tx.chain_id,
        access_list: tx.access_list.unwrap_or_default().0,
        blob_hashes: tx.blob_versioned_hashes.unwrap_or_default(),
        max_fee_per_blob_gas: tx.max_fee_per_blob_gas.map(U256::from),
        authorization_list: None, // TODO: Support in the upcoming hardfork
    }
}
