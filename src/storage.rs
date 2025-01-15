use crate::{
    scheduler::{MVMemory, RewardsAccumulator},
    AccountBasic, LocationAndType, MemoryEntry, MemoryValue, ReadVersion, TxId, TxVersion,
};
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use dashmap::DashMap;
use revm::interpreter::analysis::to_analysed;
use revm_primitives::{
    db::{Database, DatabaseRef},
    AccountInfo, Address, Bytecode, EvmState, B256, KECCAK_EMPTY, U256,
};

pub type CachedStorageData = DashMap<LocationAndType, MemoryValue>;

pub(crate) struct CacheDB<'a, DB, RA>
where
    DB: DatabaseRef,
    RA: RewardsAccumulator,
{
    coinbase: Address,
    db: &'a DB,
    cache: &'a CachedStorageData,
    mv_memory: &'a MVMemory,
    rewards_accumulator: &'a RA,

    read_set: HashMap<LocationAndType, ReadVersion>,
    read_accounts: HashMap<Address, AccountBasic>,
    current_tx: TxVersion,
    rewards_accumulated: bool,
    estimate_txs: HashSet<TxId>,
    self_destructed: bool,
}

impl<'a, DB, RA> CacheDB<'a, DB, RA>
where
    DB: DatabaseRef,
    RA: RewardsAccumulator,
{
    pub fn new(
        coinbase: Address,
        db: &'a DB,
        cache: &'a CachedStorageData,
        mv_memory: &'a MVMemory,
        rewards_accumulator: &'a RA,
    ) -> Self {
        Self {
            coinbase,
            db,
            cache,
            mv_memory,
            rewards_accumulator,
            read_set: HashMap::new(),
            read_accounts: HashMap::new(),
            current_tx: TxVersion::new(0, 0),
            rewards_accumulated: true,
            estimate_txs: HashSet::new(),
            self_destructed: false,
        }
    }

    pub fn reset_state(&mut self, tx_version: TxVersion) {
        self.current_tx = tx_version;
        self.read_set.clear();
        self.read_accounts.clear();
        self.rewards_accumulated = true;
        self.estimate_txs.clear();
        self.self_destructed = false;
    }

    pub fn rewards_accumulated(&self) -> bool {
        self.rewards_accumulated
    }

    pub fn take_estimate_txs(&mut self) -> HashSet<TxId> {
        std::mem::take(&mut self.estimate_txs)
    }

    pub fn is_selfdestructed(&self) -> bool {
        self.self_destructed
    }

    pub fn take_read_set(&mut self) -> HashMap<LocationAndType, ReadVersion> {
        std::mem::take(&mut self.read_set)
    }

    pub(crate) fn update_mv_memory(
        &self,
        changes: &EvmState,
        estimate: bool,
    ) -> HashSet<LocationAndType> {
        let mut write_set = HashSet::new();
        for (address, account) in changes {
            if account.is_selfdestructed() {
                let memory_entry = MemoryEntry::new(
                    self.current_tx.incarnation,
                    MemoryValue::SelfDestructed,
                    estimate,
                );
                self.mv_memory
                    .entry(LocationAndType::Code(address.clone()))
                    .or_default()
                    .insert(self.current_tx.txid, memory_entry.clone());
                self.mv_memory
                    .entry(LocationAndType::Basic(address.clone()))
                    .or_default()
                    .insert(self.current_tx.txid, memory_entry);
                continue;
            }

            // If the account is touched, it means that the account's state has been modified
            // during the transaction. This includes changes to the account's balance, nonce,
            // or code. We need to track these changes to ensure the correct state is committed
            // after the transaction.
            if account.is_touched() {
                let read_account = self.read_accounts.get(address);
                let has_code = !account.info.is_empty_code_hash();
                // is newly created contract
                let new_contract = has_code &&
                    account.info.code.is_some() &&
                    read_account.map_or(true, |account| account.code_hash.is_none());
                if new_contract {
                    let location = LocationAndType::Code(address.clone());
                    write_set.insert(location.clone());
                    self.mv_memory.entry(location).or_default().insert(
                        self.current_tx.txid,
                        MemoryEntry::new(
                            self.current_tx.incarnation,
                            MemoryValue::Code(to_analysed(account.info.code.clone().unwrap())),
                            estimate,
                        ),
                    );
                }

                if new_contract ||
                    read_account.is_none() ||
                    read_account.is_some_and(|basic| {
                        basic.nonce != account.info.nonce || basic.balance != account.info.balance
                    })
                {
                    let location = LocationAndType::Basic(address.clone());
                    write_set.insert(location.clone());
                    self.mv_memory.entry(location).or_default().insert(
                        self.current_tx.txid,
                        MemoryEntry::new(
                            self.current_tx.incarnation,
                            MemoryValue::Basic(AccountInfo { code: None, ..account.info }),
                            estimate,
                        ),
                    );
                }
            }

            for (slot, value) in account.changed_storage_slots() {
                let location = LocationAndType::Storage(*address, *slot);
                write_set.insert(location.clone());
                self.mv_memory.entry(location).or_default().insert(
                    self.current_tx.txid,
                    MemoryEntry::new(
                        self.current_tx.incarnation,
                        MemoryValue::Storage(value.present_value),
                        estimate,
                    ),
                );
            }
        }

        write_set
    }

    fn code_by_address(
        &mut self,
        address: Address,
        code_hash: B256,
    ) -> Result<Bytecode, DB::Error> {
        let mut result = None;
        let mut read_version = ReadVersion::Storage;
        let location = LocationAndType::Code(address);
        // 1. read from multi-version memory
        if let Some(written_transactions) = self.mv_memory.get(&location) {
            if let Some((&txid, entry)) =
                written_transactions.range(..self.current_tx.txid).next_back()
            {
                match &entry.data {
                    MemoryValue::Code(code) => {
                        result = Some(code.clone());
                        if entry.estimate {
                            self.estimate_txs.insert(txid);
                        }
                        read_version =
                            ReadVersion::MvMemory(TxVersion::new(txid, entry.incarnation));
                    }
                    MemoryValue::SelfDestructed => {
                        panic!("Can't visit self-destructed code");
                    }
                    _ => {}
                }
            }
        }
        // 2. read from database
        if result.is_none() {
            let location = LocationAndType::CodeHash(code_hash);
            if let Some(code) = self.cache.get(&location) {
                if let MemoryValue::Code(code) = code.value() {
                    result = Some(code.clone());
                }
            } else {
                let code = self.db.code_by_hash_ref(code_hash.clone())?;
                let code = to_analysed(code);
                result = Some(code.clone());
                self.cache.insert(location.clone(), MemoryValue::Code(code));
            };
        }

        self.read_set.insert(location, read_version);
        Ok(result.expect("No bytecode"))
    }
}

impl<'a, DB, RA> Database for CacheDB<'a, DB, RA>
where
    DB: DatabaseRef,
    RA: RewardsAccumulator,
{
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let mut result = None;
        let mut read_version = ReadVersion::Storage;
        let mut read_account = AccountBasic { balance: U256::ZERO, nonce: 0, code_hash: None };
        let location = LocationAndType::Basic(address.clone());
        // 1. read from multi-version memory
        let mut self_destructed = false;
        if let Some(written_transactions) = self.mv_memory.get(&location) {
            if let Some((&txid, entry)) =
                written_transactions.range(..self.current_tx.txid).next_back()
            {
                match &entry.data {
                    MemoryValue::Basic(info) => {
                        result = Some(info.clone());
                        read_account = AccountBasic {
                            balance: info.balance,
                            nonce: info.nonce,
                            code_hash: if info.is_empty_code_hash() {
                                None
                            } else {
                                Some(info.code_hash)
                            },
                        };
                        if entry.estimate {
                            self.estimate_txs.insert(txid);
                        }
                        read_version =
                            ReadVersion::MvMemory(TxVersion::new(txid, entry.incarnation));
                    }
                    MemoryValue::SelfDestructed => {
                        self.self_destructed = true;
                        self_destructed = true;
                        read_version =
                            ReadVersion::MvMemory(TxVersion::new(txid, entry.incarnation));
                    }
                    _ => {}
                }
            }
        }
        // 2. read from database
        if !self_destructed && result.is_none() {
            if let Some(info) = self.cache.get(&location) {
                if let MemoryValue::Basic(info) = info.value() {
                    read_account = AccountBasic {
                        balance: info.balance,
                        nonce: info.nonce,
                        code_hash: if info.is_empty_code_hash() {
                            None
                        } else {
                            Some(info.code_hash)
                        },
                    };
                    result = Some(info.clone());
                }
            } else {
                let info = self.db.basic_ref(address.clone())?;
                if let Some(info) = info {
                    read_account = AccountBasic {
                        balance: info.balance,
                        nonce: info.nonce,
                        code_hash: if info.is_empty_code_hash() {
                            None
                        } else {
                            Some(info.code_hash)
                        },
                    };
                    result = Some(info.clone());
                    self.cache.insert(location.clone(), MemoryValue::Basic(info));
                }
            };
        }

        let mut inc_rewards = 0;
        if address == self.coinbase {
            let start_tx = match read_version {
                ReadVersion::MvMemory(TxVersion { txid, incarnation: _incarnation }) => Some(txid),
                ReadVersion::Storage => None,
            };
            if let Some(rewards) =
                self.rewards_accumulator.accumulate(start_tx, self.current_tx.txid)
            {
                inc_rewards = rewards;
            } else {
                self.rewards_accumulated = false;
            }
        }

        if let Some(info) = &mut result {
            if !info.is_empty_code_hash() && info.code.is_none() {
                info.code = Some(self.code_by_address(address.clone(), info.code_hash)?);
            }
            if inc_rewards != 0 {
                info.balance = info.balance.saturating_add(U256::from(inc_rewards));
            }
        } else if inc_rewards != 0 {
            result = Some(AccountInfo {
                balance: U256::from(inc_rewards),
                nonce: 0,
                code_hash: KECCAK_EMPTY,
                code: None,
            });
        }

        self.read_accounts.insert(address, read_account);
        self.read_set.insert(location, read_version);
        Ok(result)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db.code_by_hash_ref(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let mut result = None;
        let mut read_version = ReadVersion::Storage;
        let location = LocationAndType::Storage(address.clone(), index.clone());
        // 1. read from multi-version memory
        if let Some(written_transactions) = self.mv_memory.get(&location) {
            if let Some((&txid, entry)) =
                written_transactions.range(..self.current_tx.txid).next_back()
            {
                if let MemoryValue::Storage(slot) = &entry.data {
                    result = Some(slot.clone());
                    if entry.estimate {
                        self.estimate_txs.insert(txid);
                    }
                    read_version = ReadVersion::MvMemory(TxVersion::new(txid, entry.incarnation));
                }
            }
        }
        // 2. read from database
        if result.is_none() {
            if let Some(slot) = self.cache.get(&location) {
                if let MemoryValue::Storage(slot) = slot.value() {
                    result = Some(slot.clone());
                }
            } else {
                let mut new_ca = false;
                if let Some(ReadVersion::MvMemory(_)) =
                    self.read_set.get(&LocationAndType::Code(address.clone()))
                {
                    new_ca = true;
                }
                let slot =
                    if new_ca { U256::default() } else { self.db.storage_ref(address, index)? };
                result = Some(slot.clone());
                self.cache.insert(location.clone(), MemoryValue::Storage(slot));
            };
        }

        self.read_set.insert(location, read_version);
        Ok(result.expect("No storage slot"))
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash_ref(number)
    }
}
