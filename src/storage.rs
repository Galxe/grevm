use crate::LocationAndType;
use revm::db::states::bundle_state::BundleRetention;
use revm::db::states::StorageSlot;
use revm::db::{AccountStatus, BundleState, StorageWithOriginalValues};
use revm::precompile::Address;
use revm::primitives::{Account, AccountInfo, Bytecode, EvmState, B256, BLOCK_HASH_HISTORY, U256};
use revm::{Database, DatabaseRef, TransitionAccount};
use std::collections::{btree_map, hash_map, BTreeMap, HashMap, HashSet};
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct StateCache {
    /// Account state.
    pub accounts: HashMap<Address, CachedAccount>,
    /// Loaded contracts.
    pub contracts: HashMap<B256, Bytecode>,
}

impl StateCache {
    fn apply_evm_state(&mut self, changes: &EvmState) {
        for (address, account) in changes {
            self.apply_account_state(address, account);
        }
    }

    /// Refer to `CacheState::apply_evm_state`
    fn apply_account_state(&mut self, address: &Address, account: &Account) {
        // not touched account are never changed.
        if !account.is_touched() {
            return;
        }

        let this_account =
            self.accounts.get_mut(address).expect("All accounts should be present inside cache");

        // If it is marked as selfdestructed inside revm
        // we need to changed state to destroyed.
        if account.is_selfdestructed() {
            return this_account.selfdestruct();
        }

        let is_created = account.is_created();
        let is_empty = account.is_empty();

        let changed_storage =
            account.storage.iter().filter(|(_, slot)| slot.is_changed()).map(|(k, slot)| {
                (
                    *k,
                    StorageSlot {
                        previous_or_original_value: slot.original_value,
                        present_value: slot.present_value,
                    },
                )
            });

        if is_created {
            this_account.newly_created(account.info.clone(), changed_storage.collect());
            return;
        }

        if is_empty {
            // TODO(gravity): has_state_clear

            // if account is empty and state clear is not enabled we should save
            // empty account.
            this_account.touch_create_pre_eip161(changed_storage.collect());
            return;
        }

        this_account.change(account.info.clone(), changed_storage.collect());
        return;
    }

    /// Merge loaded state from other as initial state.
    pub(crate) fn merge_loaded_state(&mut self, other: Self) {
        for (address, account) in other.accounts {
            if self.accounts.contains_key(&address) {
                continue;
            }

            let (info, status) = if account.status.is_not_modified() {
                (account.info, account.status)
            } else {
                (account.original_info, account.original_status)
            };
            // TODO(gravity_nekomoto): Also merge storage? However modified storage will be
            // filled by call `apply_account_state`.
            self.accounts.insert(address, CachedAccount { info, status, ..Default::default() });
        }

        for (code_hash, code) in other.contracts {
            if !self.contracts.contains_key(&code_hash) {
                self.contracts.insert(code_hash, code);
            }
        }
    }

    pub(crate) fn merge_accounts(&mut self, other: Self) {
        for (address, account) in other.accounts {
            match self.accounts.entry(address) {
                hash_map::Entry::Occupied(mut entry) => {
                    let this_account = entry.get_mut();
                    if this_account.status.is_not_modified() {
                        this_account.original_info = this_account.info.take();
                        this_account.original_status = this_account.status;
                    }
                    this_account.info = account.info;
                    this_account.status = account.status;
                    if account.storage_was_destroyed {
                        this_account.storage = account.storage;
                    } else {
                        this_account.storage.extend(
                            account.storage.into_iter().filter(|(_, slot)| slot.is_changed()),
                        );
                    }
                    this_account.storage_was_destroyed = account.storage_was_destroyed;
                }
                hash_map::Entry::Vacant(entry) => {
                    entry.insert(account);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CachedAccount {
    info: Option<AccountInfo>,
    status: AccountStatus,
    /// Only present if account is modified.
    original_info: Option<AccountInfo>,
    original_status: AccountStatus,
    storage: StorageWithOriginalValues,
    storage_was_destroyed: bool,
}

impl CachedAccount {
    /// Refer to `CacheAccount::increment_balance`
    fn increment_balance(&mut self, balance: u128) {
        let had_no_nonce_and_code =
            self.info.as_ref().map(AccountInfo::has_no_code_and_nonce).unwrap_or_default();

        if self.status.is_not_modified() {
            self.original_info = self.info.clone();
            self.original_status = self.status;
        }

        if self.info.is_none() {
            self.info = Some(AccountInfo::default());
        }

        self.status = self.status.on_changed(had_no_nonce_and_code);
        let info = self.info.as_mut().unwrap();
        info.balance = info.balance.saturating_add(U256::from(balance));
    }

    /// Refer to `CacheAccount::selfdestruct`
    fn selfdestruct(&mut self) {
        self.status = self.status.on_selfdestructed();
        if self.status.is_not_modified() {
            self.original_info = self.info.take();
            self.original_status = self.status;
        }
        self.info = None;
        self.storage = HashMap::default();
        self.storage_was_destroyed = true;
    }

    /// Refer to `CacheAccount::newly_created`
    fn newly_created(&mut self, new_info: AccountInfo, new_storage: StorageWithOriginalValues) {
        self.status = self.status.on_created();
        if self.status.is_not_modified() {
            self.original_info = self.info.take();
            self.original_status = self.status;
        }
        self.info = Some(new_info);
        self.storage = new_storage;
    }

    /// Refer to `CacheAccount::touch_create_pre_eip161`
    fn touch_create_pre_eip161(&mut self, storage: StorageWithOriginalValues) {
        let previous_status = self.status;

        let had_no_info =
            self.info.as_ref().map(|a: &AccountInfo| a.is_empty()).unwrap_or_default();
        if let Some(status) = previous_status.on_touched_created_pre_eip161(had_no_info) {
            self.status = status;
        } else {
            // account status didn't change
            return;
        }

        if previous_status.is_not_modified() {
            self.original_info = self.info.take();
            self.original_status = previous_status;
        }
        self.info = Some(AccountInfo::default());
        self.storage = storage;
    }

    /// Refer to `CacheAccount::change`
    fn change(&mut self, new: AccountInfo, storage: Vec<(U256, StorageSlot)>) {
        let had_no_nonce_and_code =
            self.info.as_ref().map(AccountInfo::has_no_code_and_nonce).unwrap_or_default();

        if self.status.is_not_modified() {
            self.original_info = self.info.take();
            self.original_status = self.status;
        }

        self.status = self.status.on_changed(had_no_nonce_and_code);
        self.info = Some(new);
        for (key, slot) in storage {
            match self.storage.entry(key) {
                hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().present_value = slot.present_value;
                }
                hash_map::Entry::Vacant(entry) => {
                    entry.insert(slot);
                }
            }
        }
    }
}

pub(crate) struct SchedulerDB<DB> {
    /// Cache the committed data of finality txns and the read-only data during execution after each
    /// round of execution. Used as the initial state for the next round of partition executors.
    /// When fall back to sequential execution, used as cached state contains both changed from evm
    /// execution and cached/loaded account/storages.
    pub cache: StateCache,
    pub database: DB,
    /// If EVM asks for block hash we will first check if they are found here.
    /// and then ask the database.
    ///
    /// This map can be used to give different values for block hashes if in case
    /// The fork block is different or some blocks are not saved inside database.
    pub block_hashes: BTreeMap<u64, B256>,
}

impl<DB> SchedulerDB<DB> {
    pub(crate) fn new(database: DB) -> Self {
        Self { cache: StateCache::default(), database, block_hashes: BTreeMap::new() }
    }

    pub(crate) fn commit(&mut self, changes: &EvmState) {
        self.cache.apply_evm_state(changes);
    }

    /// Refer to `BundleState::apply_transitions_and_create_reverts`
    #[fastrace::trace]
    pub(crate) fn create_bundle_state(&mut self, retention: BundleRetention) -> BundleState {
        let mut state = BundleState::default();

        let include_reverts = retention.includes_reverts();
        // pessimistically pre-allocate assuming _all_ accounts changed.
        let reverts_capacity = if include_reverts { self.cache.accounts.len() } else { 0 };
        let mut reverts = Vec::with_capacity(reverts_capacity);

        for (address, account) in std::mem::take(&mut self.cache.accounts) {
            if account.status.is_not_modified() {
                continue;
            }

            let transition = TransitionAccount {
                info: account.info,
                status: account.status,
                previous_info: account.original_info,
                previous_status: account.original_status,
                storage: account
                    .storage
                    .into_iter()
                    .filter(|(_, slot)| slot.is_changed())
                    .collect(),
                storage_was_destroyed: account.storage_was_destroyed,
            };

            // add new contract if it was created/changed.
            if let Some((hash, new_bytecode)) = transition.has_new_contract() {
                state.contracts.insert(hash, new_bytecode.clone());
            }

            let present_bundle = transition.present_bundle_account();
            let revert = transition.create_revert();
            if let Some(revert) = revert {
                state.state_size += present_bundle.size_hint();
                state.state.insert(address, present_bundle);
                if include_reverts {
                    reverts.push((address, revert));
                }
            }
        }

        state.reverts.push(reverts);

        state
    }
}

impl<DB> SchedulerDB<DB>
where
    DB: DatabaseRef,
{
    pub(crate) fn load_cache_account(
        &mut self,
        address: Address,
    ) -> Result<&mut CachedAccount, DB::Error> {
        match self.cache.accounts.entry(address) {
            hash_map::Entry::Vacant(entry) => {
                let info = self.database.basic_ref(address)?;
                Ok(entry.insert(into_cached_account(info)))
            }
            hash_map::Entry::Occupied(entry) => Ok(entry.into_mut()),
        }
    }

    /// rewards are distributed to the miner, ommers, and the DAO.
    /// TODO(gaoxin): Full block reward to block.beneficiary
    pub(crate) fn increment_balances(
        &mut self,
        balances: impl IntoIterator<Item = (Address, u128)>,
    ) -> Result<(), DB::Error> {
        // make transition and update cache state
        for (address, balance) in balances {
            if balance == 0 {
                continue;
            }

            let cache_account = self.load_cache_account(address)?;
            cache_account.increment_balance(balance);
        }
        Ok(())
    }
}

fn into_cached_account(account: Option<AccountInfo>) -> CachedAccount {
    match account {
        // refer to `CacheAccount::new_loaded_not_existing`
        None => CachedAccount {
            info: None,
            status: AccountStatus::LoadedNotExisting,
            ..Default::default()
        },
        // refer to `CacheAccount::new_loaded_empty_eip161`
        Some(acc) if acc.is_empty() => CachedAccount {
            info: Some(AccountInfo::default()),
            status: AccountStatus::LoadedEmptyEIP161,
            ..Default::default()
        },
        // refer to `CacheAccount::new_loaded`
        Some(acc) => {
            CachedAccount { info: Some(acc), status: AccountStatus::Loaded, ..Default::default() }
        }
    }
}

/// Get storage value of address at index.
fn load_storage<DB: DatabaseRef>(
    cache: &mut StateCache,
    database: &DB,
    address: Address,
    index: U256,
) -> Result<U256, DB::Error> {
    // Account is guaranteed to be loaded.
    // Note that storage from bundle is already loaded with account.
    if let Some(account) = cache.accounts.get_mut(&address) {
        // account will always be some, but if it is not, U256::ZERO will be returned.
        let is_storage_known = account.status.is_storage_known();
        match account.storage.entry(index) {
            hash_map::Entry::Occupied(entry) => Ok(entry.get().present_value()),
            hash_map::Entry::Vacant(entry) => {
                // if account was destroyed or account is newly built
                // we return zero and don't ask database.
                let value = if is_storage_known {
                    U256::ZERO
                } else {
                    tokio::task::block_in_place(|| database.storage_ref(address, index))?
                };
                entry.insert(StorageSlot::new(value));
                Ok(value)
            }
        }
    } else {
        unreachable!("For accessing any storage account is guaranteed to be loaded beforehand")
    }
}

/// Fall back to sequential execute
impl<DB> Database for SchedulerDB<DB>
where
    DB: DatabaseRef,
{
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.load_cache_account(address).map(|account| account.info.clone())
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        let res = match self.cache.contracts.entry(code_hash) {
            hash_map::Entry::Occupied(entry) => Ok(entry.get().clone()),
            hash_map::Entry::Vacant(entry) => {
                let code = self.database.code_by_hash_ref(code_hash)?;
                entry.insert(code.clone());
                Ok(code)
            }
        };
        res
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        load_storage(&mut self.cache, &self.database, address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        match self.block_hashes.entry(number) {
            btree_map::Entry::Occupied(entry) => Ok(*entry.get()),
            btree_map::Entry::Vacant(entry) => {
                let ret = *entry.insert(self.database.block_hash_ref(number)?);

                // prune all hashes that are older than BLOCK_HASH_HISTORY
                let last_block = number.saturating_sub(BLOCK_HASH_HISTORY);
                while let Some(entry) = self.block_hashes.first_entry() {
                    if *entry.key() < last_block {
                        entry.remove();
                    } else {
                        break;
                    }
                }

                Ok(ret)
            }
        }
    }
}

pub(crate) struct PartitionDB<DB> {
    pub coinbase: Address,

    // partition internal cache
    pub cache: StateCache,
    pub scheduler_db: Arc<SchedulerDB<DB>>,
    pub block_hashes: BTreeMap<u64, B256>,

    /// Does the miner participate in the transaction
    pub miner_involved: bool,
    /// Record the read set of current tx, will be consumed after the execution of each tx
    tx_read_set: HashSet<LocationAndType>,
}

impl<DB: DatabaseRef> PartitionDB<DB> {
    pub(crate) fn new(coinbase: Address, scheduler_db: Arc<SchedulerDB<DB>>) -> Self {
        Self {
            coinbase,
            cache: StateCache::default(),
            scheduler_db,
            block_hashes: BTreeMap::new(),
            miner_involved: false,
            tx_read_set: HashSet::new(),
        }
    }

    /// consume the read set after evm.transact() for each tx
    pub(crate) fn take_read_set(&mut self) -> HashSet<LocationAndType> {
        core::mem::take(&mut self.tx_read_set)
    }

    /// Generate the write set after evm.transact() for each tx
    pub(crate) fn generate_write_set(
        &self,
        changes: &EvmState,
    ) -> (HashSet<LocationAndType>, Option<u128>) {
        let mut rewards: Option<u128> = None;
        let mut write_set = HashSet::new();
        for (address, account) in changes {
            if account.is_selfdestructed() {
                write_set.insert(LocationAndType::Code(address.clone()));
                // When a contract account is destroyed, its remaining balance is sent to a
                // designated address, and the account’s balance becomes invalid.
                // Defensive programming should be employed to prevent subsequent transactions
                // from attempting to read the contract account’s basic information,
                // which could lead to errors.
                write_set.insert(LocationAndType::Basic(address.clone()));
                continue;
            }

            let mut miner_updated = false;
            // When fully tracking the updates to the miner’s account,
            // we should set rewards = 0
            if self.coinbase == *address && !self.miner_involved {
                match self.cache.accounts.get(address) {
                    Some(miner) => match miner.info.as_ref() {
                        Some(miner) => {
                            rewards = Some((account.info.balance - miner.balance).to());
                            miner_updated = true;
                        }
                        // LoadedNotExisting
                        None => {
                            rewards = Some(account.info.balance.to());
                            miner_updated = true;
                        }
                    },
                    None => panic!("Miner should be cached"),
                }
            }

            if account.is_touched() {
                let has_code = !account.info.is_empty_code_hash();
                // is newly created contract
                let mut new_contract_account = false;

                if match self.cache.accounts.get(address) {
                    Some(read_account) => read_account.info.as_ref().map_or(true, |read_account| {
                        new_contract_account = has_code && read_account.is_empty_code_hash();
                        new_contract_account
                            || read_account.nonce != account.info.nonce
                            || read_account.balance != account.info.balance
                    }),
                    None => {
                        new_contract_account = has_code;
                        true
                    }
                } {
                    if !miner_updated {
                        write_set.insert(LocationAndType::Basic(*address));
                    }
                }
                if new_contract_account {
                    write_set.insert(LocationAndType::Code(*address));
                }
            }

            for (slot, _) in account.changed_storage_slots() {
                write_set.insert(LocationAndType::Storage(*address, *slot));
            }
        }
        (write_set, rewards)
    }

    /// Temporary commit the state change after evm.transact() for each tx.
    pub(crate) fn temporary_commit(&mut self, changes: &EvmState) {
        self.cache.apply_evm_state(changes);
    }

    pub(crate) fn load_cache_account(
        &mut self,
        address: Address,
    ) -> Result<&mut CachedAccount, DB::Error> {
        // 1. read from internal cache
        match self.cache.accounts.entry(address) {
            hash_map::Entry::Vacant(entry) => {
                // 2. read initial state of this round from scheduler cache
                if let Some(account) = self.scheduler_db.cache.accounts.get(&address) {
                    Ok(entry.insert(CachedAccount {
                        info: account.info.clone(),
                        status: account.status,
                        storage: account.storage.clone(),
                        ..Default::default()
                    }))
                } else {
                    // 3. read from origin database
                    let info = tokio::task::block_in_place(|| {
                        self.scheduler_db.database.basic_ref(address)
                    })?;
                    Ok(entry.insert(into_cached_account(info)))
                }
            }
            hash_map::Entry::Occupied(entry) => Ok(entry.into_mut()),
        }
    }
}

/// Used to build evm, and hook the read operations
impl<DB> Database for PartitionDB<DB>
where
    DB: DatabaseRef,
{
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if address != self.coinbase || self.miner_involved {
            self.tx_read_set.insert(LocationAndType::Basic(address));
        }

        let result = self.load_cache_account(address).map(|account| account.info.clone());
        if let Ok(account) = &result {
            if let Some(info) = account {
                if !info.is_empty_code_hash() {
                    self.tx_read_set.insert(LocationAndType::Code(address));
                }
            }
        }
        result
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        // 1. read from internal cache
        let res = match self.cache.contracts.entry(code_hash) {
            hash_map::Entry::Occupied(entry) => Ok(entry.get().clone()),
            hash_map::Entry::Vacant(entry) => {
                // 2. read initial state of this round from scheduler cache
                if let Some(code) = self.scheduler_db.cache.contracts.get(&code_hash) {
                    return Ok(entry.insert(code.clone()).clone());
                }

                // 3. read from origin database
                let code = tokio::task::block_in_place(|| {
                    self.scheduler_db.database.code_by_hash_ref(code_hash)
                })?;
                entry.insert(code.clone());
                return Ok(code);
            }
        };
        res
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.tx_read_set.insert(LocationAndType::Storage(address, index));

        load_storage(&mut self.cache, &self.scheduler_db.database, address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        // FIXME(gravity_nekomoto): too lot repeated code
        match self.block_hashes.entry(number) {
            btree_map::Entry::Occupied(entry) => Ok(*entry.get()),
            btree_map::Entry::Vacant(entry) => {
                // TODO(gravity_nekomoto): read from scheduler_db?
                let ret = *entry.insert(tokio::task::block_in_place(|| {
                    self.scheduler_db.database.block_hash_ref(number)
                })?);

                // prune all hashes that are older then BLOCK_HASH_HISTORY
                let last_block = number.saturating_sub(BLOCK_HASH_HISTORY);
                while let Some(entry) = self.block_hashes.first_entry() {
                    if *entry.key() < last_block {
                        entry.remove();
                    } else {
                        break;
                    }
                }

                Ok(ret)
            }
        }
    }
}
