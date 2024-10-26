use revm::primitives::alloy_primitives::U160;
use revm::primitives::ruint::UintTryFrom;
use revm::primitives::{keccak256, Address, Bytes, TxEnv, TxKind, B256, U256};
use std::sync::Arc;

use crate::{fork_join_util, LocationAndType, SharedTxStates, TxState};

/// This module provides functionality for parsing and handling execution hints
/// for parallel transaction execution in the context of Ethereum-like blockchains.
/// It includes definitions for contract types, ERC20 functions, read-write types,
/// and methods for updating transaction states based on contract interactions.
#[allow(dead_code)]
enum ContractType {
    UNKNOWN,
    ERC20,
}

/// Represents different types of contracts. Currently, only ERC20 is supported.
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
    fn insert_location(&mut self, location: LocationAndType, rw_type: RWType) {
        match rw_type {
            RWType::ReadOnly => {
                self.read_set.insert(location, None);
            }
            RWType::WriteOnly => {
                self.write_set.insert(location);
            }
            RWType::ReadWrite => {
                self.read_set.insert(location.clone(), None);
                self.write_set.insert(location);
            }
        }
    }
}

/// Struct to hold shared transaction states and provide methods for parsing
/// and handling execution hints for parallel transaction execution.
pub(crate) struct ParallelExecutionHints {
    /// Shared transaction states that will be updated with read/write sets
    /// based on the contract interactions.
    tx_states: SharedTxStates,
}

impl ParallelExecutionHints {
    pub(crate) fn new(tx_states: SharedTxStates) -> Self {
        Self { tx_states }
    }

    /// Obtain a mutable reference to shared transaction states, and parse execution hints for each transaction.
    /// Although fork_join_util executes concurrently, transactions between each partition do not overlap.
    /// This means each partition can independently update its assigned transactions.
    /// `self.tx_states` is immutable, and can only be converted to mutable through unsafe code.
    /// An alternative approach is to declare `self.tx_states` as `Arc<Vec<Mutex<TxState>>>`, allowing mutable access via `self.tx_states[index].lock().unwrap()`.
    /// However, this would introduce locking overhead and impact performance.
    /// The primary consideration is that developers are aware there are no conflicts between transactions, making the `Mutex` approach unnecessarily verbose and cumbersome.
    #[fastrace::trace]
    pub(crate) fn parse_hints(&self, txs: Arc<Vec<TxEnv>>) {
        // Utilize fork-join utility to process transactions in parallel
        fork_join_util(txs.len(), None, |start_tx, end_tx, _| {
            #[allow(invalid_reference_casting)]
            let hints =
                unsafe { &mut *(&(*self.tx_states) as *const Vec<TxState> as *mut Vec<TxState>) };
            for index in start_tx..end_tx {
                let tx_env = &txs[index];
                let rw_set = &mut hints[index];
                // Insert caller's basic location into read-write set
                rw_set.insert_location(LocationAndType::Basic(tx_env.caller), RWType::ReadWrite);
                if let TxKind::Call(to_address) = tx_env.transact_to {
                    if !tx_env.data.is_empty() {
                        rw_set
                            .insert_location(LocationAndType::Basic(to_address), RWType::ReadOnly);
                        rw_set.insert_location(LocationAndType::Code(to_address), RWType::ReadOnly);
                        // Update hints with contract data based on the transaction details
                        if !ParallelExecutionHints::update_hints_with_contract_data(
                            tx_env.caller,
                            to_address,
                            None,
                            &tx_env.data,
                            rw_set,
                        ) {
                            rw_set.insert_location(
                                LocationAndType::Basic(to_address),
                                RWType::WriteOnly,
                            );
                        }
                    } else if to_address != tx_env.caller {
                        rw_set
                            .insert_location(LocationAndType::Basic(to_address), RWType::ReadWrite);
                    }
                }
            }
        });
    }

    /// This function computes the storage slot using the provided slot number and a vector of indices.
    /// It utilizes the keccak256 hash function to derive the final storage slot value.
    ///
    /// # Type Parameters
    ///
    /// * `K` - The type of the slot number.
    /// * `V` - The type of the indices.
    ///
    /// # Parameters
    ///
    /// * `slot` - The initial slot number.
    /// * `indices` - A vector of indices used to compute the final storage slot.
    ///
    /// # Returns
    ///
    /// * `U256` - The computed storage slot.
    ///
    /// # ABI Standards
    ///
    /// This function adheres to the ABI (Application Binary Interface) standards for Ethereum Virtual Machine (EVM).
    /// For more information on ABI standards, you can refer to the following resources:
    ///
    /// * [Ethereum Contract ABI Specification](https://docs.soliditylang.org/en/latest/abi-spec.html)
    /// * [Ethereum Yellow Paper](https://ethereum.github.io/yellowpaper/paper.pdf)
    /// * [EIP-20: ERC-20 Token Standard](https://eips.ethereum.org/EIPS/eip-20)
    ///
    /// These resources provide detailed information on how data is encoded and decoded in the EVM, which is essential for understanding how storage slots are calculated.
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
    ) -> bool {
        if code.is_none() && data.is_empty() {
            return false;
        }
        if data.len() < 4 || (data.len() - 4) % 32 != 0 {
            // Invalid tx, or tx that triggers fallback CALL
            return false;
        }
        let (func_id, parameters) = Self::decode_contract_parameters(data);
        match Self::get_contract_type(contract_address) {
            ContractType::ERC20 => match ERC20Function::from(func_id) {
                ERC20Function::Transfer => {
                    if parameters.len() != 2 {
                        return false;
                    }
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
                    if parameters.len() != 3 {
                        return false;
                    }
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
                    return false;
                }
            },
            ContractType::UNKNOWN => {
                return false;
            }
        }
        true
    }

    fn get_contract_type(_contract_address: Address) -> ContractType {
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
