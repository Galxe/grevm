use std::collections::HashMap;

use revm::{
    db::PlainAccount,
    primitives::{
        alloy_primitives::U160, keccak256, ruint::UintTryFrom, AccountInfo, Address, Bytecode,
        B256, I256, U256,
    },
    DatabaseRef,
};

/// A DatabaseRef that stores chain data in memory.
#[derive(Debug, Default, Clone)]
pub struct InMemoryDB {
    pub accounts: HashMap<Address, PlainAccount>,
    pub bytecodes: HashMap<B256, Bytecode>,
    pub block_hashes: HashMap<u64, B256>,
    /// Simulated query latency in microseconds
    pub latency_us: u64,
}

impl InMemoryDB {
    pub fn new(
        accounts: HashMap<Address, PlainAccount>,
        bytecodes: HashMap<B256, Bytecode>,
        block_hashes: HashMap<u64, B256>,
    ) -> Self {
        Self { accounts, bytecodes, block_hashes, latency_us: 0 }
    }
}

impl DatabaseRef for InMemoryDB {
    type Error = String;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if self.latency_us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(self.latency_us));
        }
        Ok(self.accounts.get(&address).map(|account| account.info.clone()))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if self.latency_us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(self.latency_us));
        }
        self.bytecodes
            .get(&code_hash)
            .cloned()
            .ok_or(String::from(format!("can't find code by hash {code_hash}")))
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if self.latency_us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(self.latency_us));
        }
        let storage = self.accounts.get(&address).ok_or(format!("can't find account {address}"))?;
        Ok(storage.storage.get(&index).cloned().unwrap_or_default())
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        if self.latency_us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(self.latency_us));
        }
        Ok(self
            .block_hashes
            .get(&number)
            .cloned()
            // Matching REVM's [EmptyDB] for now
            .unwrap_or_else(|| keccak256(number.to_string().as_bytes())))
    }
}

#[derive(Debug, Default)]
pub struct StorageBuilder {
    dict: revm_primitives::HashMap<U256, U256>,
}

impl StorageBuilder {
    pub fn new() -> Self {
        StorageBuilder { dict: HashMap::default() }
    }

    pub fn set<K, V>(&mut self, slot: K, value: V)
    where
        U256: UintTryFrom<K>,
        U256: UintTryFrom<V>,
    {
        self.dict.insert(U256::from(slot), U256::from(value));
    }

    pub fn set_many<K: Copy, const L: usize>(&mut self, starting_slot: K, value: &[U256; L])
    where
        U256: UintTryFrom<K>,
        U256: UintTryFrom<usize>,
    {
        for (index, item) in value.iter().enumerate() {
            let slot = U256::from(starting_slot).wrapping_add(U256::from(index));
            self.dict.insert(slot, *item);
        }
    }

    pub fn set_with_offset<K: Copy, V>(&mut self, key: K, offset: usize, length: usize, value: V)
    where
        U256: UintTryFrom<K>,
        U256: UintTryFrom<V>,
    {
        let entry = self.dict.entry(U256::from(key)).or_default();
        let mut buffer = B256::from(*entry);
        let value_buffer = B256::from(U256::from(value));
        buffer[(32 - offset - length)..(32 - offset)]
            .copy_from_slice(&value_buffer[(32 - length)..32]);
        *entry = buffer.into();
    }

    pub fn build(self) -> revm_primitives::HashMap<U256, U256> {
        self.dict
    }
}

pub fn from_address(address: Address) -> U256 {
    let encoded_as_u160: U160 = address.into();
    U256::from(encoded_as_u160)
}

pub fn from_short_string(text: &str) -> U256 {
    assert!(text.len() < 32);
    let encoded_as_b256 = B256::bit_or(
        B256::right_padding_from(text.as_bytes()),
        B256::left_padding_from(&[(text.len() * 2) as u8]),
    );
    encoded_as_b256.into()
}

pub fn from_indices<K, V: Copy>(slot: K, indices: &[V]) -> U256
where
    U256: UintTryFrom<K>,
    U256: UintTryFrom<V>,
{
    let mut result = B256::from(U256::from(slot));
    for index in indices {
        let to_prepend = B256::from(U256::from(*index));
        result = keccak256([to_prepend.as_slice(), result.as_slice()].concat())
    }
    result.into()
}

pub fn from_tick(tick: i32) -> U256 {
    let encoded_as_i256 = I256::try_from(tick).unwrap();
    encoded_as_i256.into_raw()
}
