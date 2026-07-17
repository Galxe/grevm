use crate::{
    AccountBasic, LocationAndType, MVMemory, MemoryEntry, MemoryValue, ReadVersion, TxId, TxVersion,
};
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use revm::{Database, DatabaseRef};
use revm_primitives::{Address, B256, U256, hardfork::SpecId};
use revm_state::{AccountInfo, Bytecode, EvmState};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
pub(crate) struct CacheDB<'a, DB>
where
    DB: DatabaseRef,
{
    spec: SpecId,
    coinbase: Address,
    db: &'a DB,
    mv_memory: &'a MVMemory,
    commit_idx: &'a AtomicUsize,

    read_set: HashMap<LocationAndType, ReadVersion>,
    read_accounts: HashMap<Address, AccountBasic>,
    current_tx: TxVersion,
    accurate_origin: bool,
    estimate_txs: HashSet<TxId>,
}

impl<'a, DB> CacheDB<'a, DB>
where
    DB: DatabaseRef,
{
    pub(crate) fn new(
        spec: SpecId,
        coinbase: Address,
        db: &'a DB,
        mv_memory: &'a MVMemory,
        commit_idx: &'a AtomicUsize,
    ) -> Self {
        Self {
            spec,
            coinbase,
            db,
            mv_memory,
            commit_idx,
            read_set: HashMap::new(),
            read_accounts: HashMap::new(),
            current_tx: TxVersion::new(0, 0),
            accurate_origin: true,
            estimate_txs: HashSet::new(),
        }
    }

    /// Whether `address` had its storage cleared earlier in this block by being (re)created
    /// (`CREATE`/`CREATE2`), in which case an unwritten slot reads as zero rather than from the
    /// base database.
    ///
    /// This is recorded on the in-block `Code` write (see [`MemoryValue::Code`]). It deliberately
    /// excludes an account whose code was merely re-pointed in-block — notably an EIP-7702 EOA
    /// being (re)delegated — which keeps its pre-existing storage and must be read from the
    /// database. (Self-destruct, which also clears storage, publishes its own
    /// [`MemoryValue::SelfDestructed`] marker and any subsequent `CREATE2` re-creation sets this
    /// flag again.)
    fn storage_cleared_in_block(&self, address: Address) -> bool {
        self.mv_memory.get(&LocationAndType::Code(address)).is_some_and(|entries| {
            matches!(
                entries.range(..self.current_tx.txid).next_back(),
                Some((_, MemoryEntry { data: MemoryValue::Code(_, true), .. }))
            )
        })
    }

    pub(crate) fn reset_state(&mut self, tx_version: TxVersion) {
        self.current_tx = tx_version;
        self.read_set.clear();
        self.read_accounts.clear();
        self.accurate_origin = true;
        self.estimate_txs.clear();
    }

    pub(crate) fn read_accurate_origin(&self) -> bool {
        self.accurate_origin
    }

    pub(crate) fn take_estimate_txs(&mut self) -> HashSet<TxId> {
        std::mem::take(&mut self.estimate_txs)
    }

    pub(crate) fn take_read_set(&mut self) -> HashMap<LocationAndType, ReadVersion> {
        std::mem::take(&mut self.read_set)
    }

    pub(crate) fn update_mv_memory(
        &self,
        changes: &EvmState,
        estimate: bool,
    ) -> HashSet<LocationAndType> {
        let mut write_set = HashSet::new();
        for (address, account) in changes {
            if *address == self.coinbase {
                continue;
            }
            // Storage-clearing account change (self-destruct, or EIP-161 empty). We mirror revm's
            // spec-aware result instead of re-deriving it: revm flags `is_selfdestructed()` only
            // when the fork truly deletes the account — pre-Cancun for any contract, post-Cancun
            // (EIP-6780) only one created in the same tx. A post-Cancun self-destruct of a
            // pre-existing account is a mere balance transfer (not flagged), so it keeps its
            // storage via the path below. Both fork semantics thus work with no
            // branching of our own.
            if account.is_selfdestructed() || account.state_clear_aware_is_empty(self.spec) {
                let memory_entry = MemoryEntry::new(
                    self.current_tx.incarnation,
                    MemoryValue::SelfDestructed,
                    estimate,
                );
                write_set.insert(LocationAndType::Basic(*address));
                self.mv_memory
                    .entry(LocationAndType::Basic(*address))
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
                // The account's code was set or changed in this transaction. Besides the usual
                // EOA/CREATE "code appears for the first time" case, EIP-7702 lets an account's
                // delegation be re-pointed in a later transaction of the same block, so the
                // post-state `code_hash` can move from one non-empty value to another. We must
                // (re)publish the `Code` entry whenever the post-state code_hash differs from what
                // we read; otherwise later txs in the block resolve a stale delegation target from
                // multi-version memory (it had only the first delegation's `Code` entry).
                let code_changed = has_code &&
                    account.info.code.is_some() &&
                    read_account
                        .map_or(true, |basic| basic.code_hash != Some(account.info.code_hash));
                if code_changed {
                    // Record whether this code write *created* the account (`CREATE`/`CREATE2`),
                    // which clears its storage, versus merely re-pointing it (an EIP-7702
                    // (re)delegation), which preserves the account's pre-existing storage. Later
                    // txs use this to decide whether an unwritten slot reads as zero or from the
                    // base database (see `storage_cleared_in_block`).
                    let location = LocationAndType::Code(address.clone());
                    write_set.insert(location.clone());
                    self.mv_memory.entry(location).or_default().insert(
                        self.current_tx.txid,
                        MemoryEntry::new(
                            self.current_tx.incarnation,
                            MemoryValue::Code(
                                account.info.code.clone().unwrap(),
                                account.is_created(),
                            ),
                            estimate,
                        ),
                    );
                }

                if code_changed ||
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
                    MemoryValue::Code(code, _) => {
                        result = Some(code.clone());
                        if entry.estimate {
                            self.estimate_txs.insert(txid);
                        }
                        read_version =
                            ReadVersion::MvMemory(TxVersion::new(txid, entry.incarnation));
                    }
                    _ => {}
                }
            }
        }
        // 2. read from database
        if result.is_none() {
            let byte_code = self.db.code_by_hash_ref(code_hash)?;
            result = Some(byte_code);
        }

        self.read_set.insert(location, read_version);
        Ok(result.expect("No bytecode"))
    }

    fn clear_destructed_entry(&self, account: Address) {
        let current_tx = self.current_tx.txid;
        for mut entry in self.mv_memory.iter_mut() {
            let destructed = match entry.key() {
                LocationAndType::Basic(address) => *address == account,
                LocationAndType::Storage(address, _) => *address == account,
                LocationAndType::Code(address) => *address == account,
            };
            if destructed {
                *entry.value_mut() = entry.value_mut().split_off(&current_tx);
            }
        }
    }
}

impl<'a, DB> Database for CacheDB<'a, DB>
where
    DB: DatabaseRef,
{
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let mut result = None;
        if address == self.coinbase {
            self.accurate_origin = self.commit_idx.load(Ordering::Acquire) == self.current_tx.txid;
            result = self.db.basic_ref(address)?;
        } else {
            let mut read_version = ReadVersion::Storage;
            let mut read_account = AccountBasic { balance: U256::ZERO, nonce: 0, code_hash: None };
            let location = LocationAndType::Basic(address.clone());
            // 1. read from multi-version memory
            let mut clear_destructed_entry = false;
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
                            if self.commit_idx.load(Ordering::Acquire) == self.current_tx.txid {
                                // make sure read after the latest self-destructed
                                clear_destructed_entry = true;
                            } else {
                                self.accurate_origin = false;
                                result = Some(AccountInfo::default());
                            }
                        }
                        _ => {}
                    }
                }
            }
            if clear_destructed_entry {
                self.clear_destructed_entry(address);
            }
            // 2. read from database
            if result.is_none() {
                let info = self.db.basic_ref(address)?;
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
                }
            }
            if result.is_some() {
                self.read_accounts.insert(address, read_account);
            }
            self.read_set.insert(location, read_version);
        }

        if let Some(info) = &mut result {
            if !info.is_empty_code_hash() && info.code.is_none() {
                info.code = Some(self.code_by_address(address.clone(), info.code_hash)?);
            }
        }
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
                    result = Some(*slot);
                    if entry.estimate {
                        self.estimate_txs.insert(txid);
                    }
                    read_version = ReadVersion::MvMemory(TxVersion::new(txid, entry.incarnation));
                }
            }
        }
        // 2. read from database
        if result.is_none() {
            let slot = if self.storage_cleared_in_block(address) {
                U256::default()
            } else {
                self.db.storage_ref(address, index)?
            };
            result = Some(slot);
        }

        self.read_set.insert(location, read_version);
        Ok(result.expect("No storage slot"))
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash_ref(number)
    }
}
