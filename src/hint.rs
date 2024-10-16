use revm::primitives::alloy_primitives::U160;
use revm::primitives::ruint::UintTryFrom;
use revm::primitives::{keccak256, Address, Bytes, TxEnv, TxKind, B256, U256};
use std::sync::Arc;

use crate::{fork_join_util, LocationAndType, SharedTxStates, TxState};

enum ContractType {
    UNKNOWN,
    ERC20,
}

enum ERC20Function {
    UNKNOWN,
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

impl From<u32> for ERC20Function {
    fn from(func_id: u32) -> Self {
        match func_id {
            0xdd62ed3e => ERC20Function::Allowance,
            0x095ea7b3 => ERC20Function::Approve,
            0x70a08231 => ERC20Function::BalanceOf,
            0x313ce567 => ERC20Function::Decimals,
            0xa457c2d7 => ERC20Function::DecreaseAllowance,
            0x39509351 => ERC20Function::IncreaseAllowance,
            0x06fdde03 => ERC20Function::Name,
            0x95d89b41 => ERC20Function::Symbol,
            0x18160ddd => ERC20Function::TotalSupply,
            0xa9059cbb => ERC20Function::Transfer,
            0x23b872dd => ERC20Function::TransferFrom,
            _ => ERC20Function::UNKNOWN,
        }
    }
}

enum RWType {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

impl TxState {
    pub(crate) fn insert_location(&mut self, location: LocationAndType, rw_type: RWType) {
        match rw_type {
            RWType::ReadOnly => {
                self.read_set.insert(location);
            }
            RWType::WriteOnly => {
                self.write_set.insert(location);
            }
            RWType::ReadWrite => {
                self.read_set.insert(location.clone());
                self.write_set.insert(location);
            }
        }
    }
}

pub struct ParallelExecutionHints {
    tx_states: SharedTxStates,
}

impl ParallelExecutionHints {
    pub fn new(tx_states: SharedTxStates) -> Self {
        Self { tx_states }
    }

    #[fastrace::trace]
    pub(crate) fn parse_hints(&self, txs: Arc<Vec<TxEnv>>) {
        fork_join_util(txs.len(), None, |start_txs, end_txs, _| {
            #[allow(invalid_reference_casting)]
            let hints =
                unsafe { &mut *(&(*self.tx_states) as *const Vec<TxState> as *mut Vec<TxState>) };
            for index in start_txs..end_txs {
                let tx_env = &txs[index];
                let rw_set = &mut hints[index];
                // Causing transactions that call the same contract to inevitably
                // conflict with each other. Is this behavior reasonable?
                // TODO(gaoxin): optimize contract account
                rw_set.insert_location(LocationAndType::Basic(tx_env.caller), RWType::ReadWrite);
                if let TxKind::Call(to_address) = tx_env.transact_to {
                    if !tx_env.data.is_empty() {
                        rw_set
                            .insert_location(LocationAndType::Basic(to_address), RWType::ReadOnly);
                        rw_set.insert_location(LocationAndType::Code(to_address), RWType::ReadOnly);
                        ParallelExecutionHints::update_hints_with_contract_data(
                            tx_env.caller,
                            to_address,
                            None,
                            &tx_env.data,
                            rw_set,
                        );
                    } else if to_address != tx_env.caller {
                        rw_set
                            .insert_location(LocationAndType::Basic(to_address), RWType::ReadWrite);
                    }
                }
            }
        });
    }

    fn slot_from_indices<K, V>(slot: K, indices: Vec<V>) -> U256
    where
        U256: UintTryFrom<K>,
        U256: UintTryFrom<V>,
    {
        let mut result = B256::from(U256::from(slot));
        for index in indices {
            let to_prepend = B256::from(U256::from(index));
            result = keccak256([to_prepend.as_slice(), result.as_slice()].concat())
        }
        result.into()
    }

    fn slot_from_address(slot: u32, addresses: Vec<Address>) -> U256 {
        let indices: Vec<U256> = addresses
            .into_iter()
            .map(|address| {
                let encoded_as_u160: U160 = address.into();
                U256::from(encoded_as_u160)
            })
            .collect();
        Self::slot_from_indices(slot, indices)
    }

    fn update_hints_with_contract_data(
        caller: Address,
        contract_address: Address,
        code: Option<Bytes>,
        data: &Bytes,
        tx_rw_set: &mut TxState,
    ) {
        if code.is_none() && data.is_empty() {
            panic!("Unreachable error")
        }
        if data.len() < 4 || (data.len() - 4) % 32 != 0 {
            tx_rw_set.insert_location(LocationAndType::Basic(contract_address), RWType::WriteOnly);
            // Invalid tx, or tx that triggers fallback CALL
            return;
        }
        let (func_id, parameters) = Self::decode_contract_parameters(data);
        match Self::get_contract_type(contract_address) {
            ContractType::ERC20 => match ERC20Function::from(func_id) {
                ERC20Function::Transfer => {
                    assert_eq!(parameters.len(), 2);
                    let to_address: [u8; 20] =
                        parameters[0].as_slice()[12..].try_into().expect("try into failed");
                    let to_slot = Self::slot_from_address(0, vec![Address::new(to_address)]);
                    let from_slot = Self::slot_from_address(0, vec![caller]);
                    tx_rw_set.insert_location(
                        LocationAndType::Storage(contract_address, from_slot),
                        RWType::ReadWrite,
                    );
                    if to_slot != from_slot {
                        tx_rw_set.insert_location(
                            LocationAndType::Storage(contract_address, to_slot),
                            RWType::ReadWrite,
                        );
                    }
                }
                ERC20Function::TransferFrom => {
                    assert_eq!(parameters.len(), 3);
                    let from_address: [u8; 20] =
                        parameters[0].as_slice()[12..].try_into().expect("try into failed");
                    let from_address = Address::new(from_address);
                    let to_address: [u8; 20] =
                        parameters[1].as_slice()[12..].try_into().expect("try into failed");
                    let to_address = Address::new(to_address);
                    let from_slot = Self::slot_from_address(1, vec![from_address]);
                    let to_slot = Self::slot_from_address(1, vec![to_address]);
                    let allowance_slot = Self::slot_from_address(1, vec![from_address, caller]);
                    tx_rw_set.insert_location(
                        LocationAndType::Storage(contract_address, from_slot),
                        RWType::ReadWrite,
                    );
                    tx_rw_set.insert_location(
                        LocationAndType::Storage(contract_address, to_slot),
                        RWType::ReadWrite,
                    );
                    // TODO(gaoxin): if from_slot == to_slot, what happened?
                    tx_rw_set.insert_location(
                        LocationAndType::Storage(contract_address, allowance_slot),
                        RWType::ReadWrite,
                    );
                }
                _ => {
                    tx_rw_set.insert_location(
                        LocationAndType::Basic(contract_address),
                        RWType::WriteOnly,
                    );
                }
            },
            ContractType::UNKNOWN => {
                tx_rw_set
                    .insert_location(LocationAndType::Basic(contract_address), RWType::WriteOnly);
            }
        }
    }

    fn get_contract_type(contract_address: Address) -> ContractType {
        // TODO(gaoxin): Parse the correct contract type to determined how to handle call data
        ContractType::ERC20
    }

    fn decode_contract_parameters(data: &Bytes) -> (u32, Vec<B256>) {
        let func_id: u32 = 0;
        let mut parameters: Vec<B256> = Vec::new();
        if data.len() <= 4 {
            return (func_id, parameters);
        }

        let func_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);

        for chunk in data[4..].chunks(32) {
            let array: [u8; 32] = chunk.try_into().expect("Slice has wrong length!");
            parameters.push(B256::from(array));
        }

        (func_id, parameters)
    }
}
