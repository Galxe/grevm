use core::hash::{BuildHasherDefault, Hasher};
use dashmap::{DashMap, Entry, mapref::one::RefMut};
use metrics::histogram;
use revm::{Database, DatabaseCommit, DatabaseRef};
use revm_database::{
    AccountStatus, BundleState, CacheState, PlainAccount, StorageWithOriginalValues,
    TransitionAccount, TransitionState,
    states::{CacheAccount, bundle_state::BundleRetention, plain_account::PlainStorage},
};
use revm_primitives::{Address, B256, HashMap, U256};
use revm_state::{Account, AccountInfo, Bytecode, EvmState};
use std::{fmt::Formatter, time::Instant, vec::Vec};

#[derive(Clone, Debug, Default)]
pub struct CacheAccountInfo {
    pub account: Option<AccountInfo>,
    pub status: AccountStatus,
}

impl CacheAccountInfo {
    pub fn new(account: Option<AccountInfo>, status: AccountStatus) -> Self {
        Self { account, status }
    }

    /// Increment balance by `balance` amount. Assume that balance will not
    /// overflow or be zero.
    ///
    /// Note: only if balance is zero we would return None as no transition would be made.
    pub fn increment_balance(&mut self, balance: u128) -> Option<TransitionAccount> {
        if balance == 0 {
            return None;
        }
        let (_, transition) = self.account_info_change(|info| {
            info.balance = info.balance.saturating_add(U256::from(balance));
        });
        Some(transition)
    }

    /// Drain balance from account and return drained amount and transition.
    ///
    /// Used for DAO hardfork transition.
    pub fn drain_balance(&mut self) -> (u128, TransitionAccount) {
        self.account_info_change(|info| {
            let output = info.balance;
            info.balance = U256::ZERO;
            output.try_into().unwrap()
        })
    }

    fn account_info_change<T, F: FnOnce(&mut AccountInfo) -> T>(
        &mut self,
        change: F,
    ) -> (T, TransitionAccount) {
        let previous_status = self.status.clone();
        let previous_info = self.account.clone();
        let mut info = self.account.take().unwrap_or_default();
        let output = change(&mut info);
        self.account = Some(info);

        let had_no_nonce_and_code =
            previous_info.as_ref().map(AccountInfo::has_no_code_and_nonce).unwrap_or_default();
        self.status = self.status.on_changed(had_no_nonce_and_code);

        (
            output,
            TransitionAccount {
                info: self.account.clone(),
                status: self.status.clone(),
                previous_info,
                previous_status,
                storage: Default::default(),
                storage_was_destroyed: false,
            },
        )
    }

    /// Consume self and make account as destroyed.
    ///
    /// Set account as None and set status to Destroyer or DestroyedAgain.
    pub fn selfdestruct(&mut self) -> Option<TransitionAccount> {
        // account should be None after selfdestruct so we can take it.
        let previous_info = self.account.take();
        let previous_status = self.status.clone();

        self.status = self.status.on_selfdestructed();

        if previous_status == AccountStatus::LoadedNotExisting {
            None
        } else {
            Some(TransitionAccount {
                info: None,
                status: self.status.clone(),
                previous_info,
                previous_status,
                storage: Default::default(),
                storage_was_destroyed: true,
            })
        }
    }

    /// Newly created account.
    pub fn newly_created(
        &mut self,
        new_info: AccountInfo,
        new_storage: StorageWithOriginalValues,
    ) -> (TransitionAccount, PlainStorage) {
        let previous_info = self.account.take();
        let previous_status = self.status.clone();

        let new_bundle_storage = new_storage.iter().map(|(k, s)| (*k, s.present_value)).collect();

        self.status = self.status.on_created();
        let transition_account = TransitionAccount {
            info: Some(new_info.clone()),
            status: self.status.clone(),
            previous_status,
            previous_info,
            storage: new_storage,
            storage_was_destroyed: false,
        };
        self.account = Some(new_info);
        (transition_account, new_bundle_storage)
    }

    /// Touch empty account, related to EIP-161 state clear.
    ///
    /// This account returns the Transition that is used to create the BundleState.
    pub fn touch_empty_eip161(&mut self) -> Option<TransitionAccount> {
        // Set account to None.
        let previous_info = self.account.take();
        let previous_status = self.status.clone();

        // Set account state to Destroyed as we need to clear the storage if it exist.
        self.status = self.status.on_touched_empty_post_eip161();

        if matches!(
            previous_status,
            AccountStatus::LoadedNotExisting |
                AccountStatus::Destroyed |
                AccountStatus::DestroyedAgain
        ) {
            None
        } else {
            Some(TransitionAccount {
                info: None,
                status: self.status.clone(),
                previous_info,
                previous_status,
                storage: Default::default(),
                storage_was_destroyed: true,
            })
        }
    }

    /// Account got touched and before EIP161 state clear this account is considered created.
    pub fn touch_create_pre_eip161(
        &mut self,
        storage: StorageWithOriginalValues,
    ) -> (Option<TransitionAccount>, PlainStorage) {
        let previous_status = self.status.clone();

        let had_no_info = self.account.as_ref().map(|info| info.is_empty()).unwrap_or_default();
        match self.status.on_touched_created_pre_eip161(had_no_info) {
            None => return (None, PlainStorage::default()),
            Some(new_status) => {
                self.status = new_status;
            }
        }

        let plain_storage = storage.iter().map(|(k, v)| (*k, v.present_value)).collect();
        let previous_info = self.account.take();

        self.account = Some(AccountInfo::default());

        (
            Some(TransitionAccount {
                info: Some(AccountInfo::default()),
                status: self.status.clone(),
                previous_info,
                previous_status,
                storage,
                storage_was_destroyed: false,
            }),
            plain_storage,
        )
    }

    pub fn change(
        &mut self,
        new: AccountInfo,
        storage: StorageWithOriginalValues,
    ) -> (TransitionAccount, PlainStorage) {
        let previous_info = self.account.take();
        let previous_status = self.status.clone();
        let new_bundle_storage = storage.iter().map(|(k, s)| (*k, s.present_value)).collect();

        let had_no_nonce_and_code =
            previous_info.as_ref().map(AccountInfo::has_no_code_and_nonce).unwrap_or_default();
        self.status = self.status.on_changed(had_no_nonce_and_code);
        self.account = Some(new);

        (
            TransitionAccount {
                info: self.account.clone(),
                status: self.status.clone(),
                previous_info,
                previous_status,
                storage,
                storage_was_destroyed: false,
            },
            new_bundle_storage,
        )
    }
}

/// Cache state contains both modified and original values.
///
/// Cache state is main state that revm uses to access state.
/// It loads all accounts from database and applies revm output to it.
///
/// It generates transitions that is used to build BundleState.
#[derive(Clone, Debug)]
pub struct ParallelCacheState {
    /// Cached accounts
    pub accounts: DashMap<Address, CacheAccountInfo>,
    /// Cached storage slots
    pub storage: DashMap<Address, DashMap<U256, U256>>,
    /// Cache contracts
    pub contracts: DashMap<B256, Bytecode>,
    /// Has EIP-161 state clear enabled (Spurious Dragon hardfork).
    pub has_state_clear: bool,
}

impl Default for ParallelCacheState {
    fn default() -> Self {
        Self::new(true)
    }
}

impl ParallelCacheState {
    /// New default state.
    pub fn new(has_state_clear: bool) -> Self {
        Self {
            accounts: Default::default(),
            storage: Default::default(),
            contracts: Default::default(),
            has_state_clear,
        }
    }

    /// Copy the cached data and convert to CacheState
    pub fn as_cache_state(&self) -> CacheState {
        let mut state = CacheState::new(self.has_state_clear);
        for kv in self.accounts.iter() {
            let info = kv.value();
            state.accounts.insert(
                kv.key().clone(),
                CacheAccount {
                    account: info
                        .account
                        .clone()
                        .map(|info| PlainAccount { info, storage: PlainStorage::default() }),
                    status: info.status.clone(),
                },
            );
        }
        for kv in self.contracts.iter() {
            state.contracts.insert(kv.key().clone(), kv.value().clone());
        }
        for kv in self.storage.iter() {
            let address = kv.key().clone();
            let slots = kv.value();
            if let Some(account) = state.accounts.get_mut(&address) {
                if let Some(plain_account) = account.account.as_mut() {
                    for slot_value in slots.iter() {
                        plain_account
                            .storage
                            .insert(slot_value.key().clone(), slot_value.value().clone());
                    }
                }
            }
        }
        state
    }

    /// Set state clear flag. EIP-161.
    pub fn set_state_clear_flag(&mut self, has_state_clear: bool) {
        self.has_state_clear = has_state_clear;
    }

    /// Insert not existing account.
    pub fn insert_not_existing(&self, address: Address) {
        self.accounts
            .insert(address, CacheAccountInfo::new(None, AccountStatus::LoadedNotExisting));
    }

    /// Insert Loaded (Or LoadedEmptyEip161 if account is empty) account.
    pub fn insert_account(&self, address: Address, info: AccountInfo) {
        let account = if !info.is_empty() {
            CacheAccountInfo::new(Some(info), AccountStatus::Loaded)
        } else {
            CacheAccountInfo::new(Some(AccountInfo::default()), AccountStatus::LoadedEmptyEIP161)
        };
        self.accounts.insert(address, account);
    }

    /// Similar to `insert_account` but with storage.
    pub fn insert_account_with_storage(
        &self,
        address: Address,
        info: AccountInfo,
        storage: PlainStorage,
    ) {
        self.update_storage_slot(address, storage);
        self.insert_account(address, info);
    }

    /// Apply output of revm execution and create account transitions that are used to build
    /// BundleState.
    pub fn apply_evm_state(&mut self, evm_state: EvmState) -> Vec<(Address, TransitionAccount)> {
        let mut transitions = Vec::with_capacity(evm_state.len());
        for (address, account) in evm_state {
            if let Some(transition) = self.apply_account_state(address, account) {
                transitions.push((address, transition));
            }
        }
        transitions
    }

    fn get_account_mut(&'_ self, address: Address) -> RefMut<'_, Address, CacheAccountInfo> {
        self.accounts.get_mut(&address).expect("All accounts should be present inside cache")
    }

    /// Apply updated account state to the cached account.
    /// Returns account transition if applicable.
    fn apply_account_state(&self, address: Address, account: Account) -> Option<TransitionAccount> {
        // not touched account are never changed.
        if !account.is_touched() {
            return None;
        }
        let is_created = account.is_created();
        let is_empty = account.is_empty();
        let is_destructed = account.is_selfdestructed();
        // transform evm storage to storage with previous value.
        let changed_storage = account
            .storage
            .into_iter()
            .filter(|(_, slot)| slot.is_changed())
            .map(|(key, slot)| (key, slot.into()))
            .collect();

        let (transition, changed_slots) = {
            // If it is marked as selfdestructed inside revm
            // we need to changed state to destroyed.
            if is_destructed {
                self.storage.remove(&address);
                return self.get_account_mut(address).selfdestruct();
            }

            // Note: it can happen that created contract get selfdestructed in same block
            // that is why is_created is checked after selfdestructed
            //
            // Note: Create2 opcode (Petersburg) was after state clear EIP (Spurious Dragon)
            //
            // Note: It is possibility to create KECCAK_EMPTY contract with some storage
            // by just setting storage inside CRATE constructor. Overlap of those contracts
            // is not possible because CREATE2 is introduced later.
            if is_created {
                let info = account.info;
                self.storage.remove(&address);
                let (transition, changed_slots) =
                    self.get_account_mut(address).newly_created(info.clone(), changed_storage);
                self.contracts.entry(info.code_hash).or_insert_with(|| info.code.clone().unwrap());
                (Some(transition), Some(changed_slots))
            }
            // Account is touched, but not selfdestructed or newly created.
            // Account can be touched and not changed.
            // And when empty account is touched it needs to be removed from database.
            // EIP-161 state clear
            else if is_empty {
                self.storage.remove(&address);
                if self.has_state_clear {
                    // touch empty account.
                    (self.get_account_mut(address).touch_empty_eip161(), None)
                } else {
                    // if account is empty and state clear is not enabled we should save
                    // empty account.
                    let (transition, changed_slots) =
                        self.get_account_mut(address).touch_create_pre_eip161(changed_storage);
                    (transition, Some(changed_slots))
                }
            } else {
                let (transition, changed_slots) =
                    self.get_account_mut(address).change(account.info, changed_storage);
                (Some(transition), Some(changed_slots))
            }
        };
        if let Some(changed_slots) = changed_slots {
            if !changed_slots.is_empty() {
                self.update_storage_slot(address, changed_slots);
            }
        }
        transition
    }

    fn update_storage_slot(&self, address: Address, storage: PlainStorage) {
        if let Some(slots) = self.storage.get(&address) {
            for (slot, value) in storage {
                slots.insert(slot, value);
            }
        } else {
            match self.storage.entry(address) {
                Entry::Occupied(entry) => {
                    for (slot, value) in storage.into_iter() {
                        entry.get().insert(slot, value);
                    }
                }
                Entry::Vacant(entry) => {
                    let new_storage = DashMap::new();
                    for (slot, value) in storage.into_iter() {
                        new_storage.insert(slot, value);
                    }
                    entry.insert(new_storage);
                }
            };
        }
    }
}

#[derive(Debug, Default)]
pub struct IdentityHasher(u64);
impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, _: &[u8]) {
        unreachable!()
    }
    fn write_u64(&mut self, id: u64) {
        self.0 = id;
    }
    fn write_usize(&mut self, id: usize) {
        self.0 = id as u64;
    }
}
pub(crate) type BuildIdentityHasher = BuildHasherDefault<IdentityHasher>;

/// State of blockchain.
///
/// State clear flag is set inside CacheState and by default it is enabled.
/// If you want to disable it use `set_state_clear_flag` function.
///
/// Represents the state of a parallelized execution environment, managing
/// cache, database interactions, and state transitions.
///
/// # Type Parameters
/// - `DB`: A type that implements the `DatabaseRef` trait, representing the database backend.
///
/// This struct provides methods for managing account balances, applying
/// transitions, and interacting with the underlying database. It also supports
/// metrics collection for database operations.
pub struct ParallelState<DB> {
    /// Cached state contains both changed from evm execution and cached/loaded account/storages
    /// from database. This allows us to have only one layer of cache where we can fetch data.
    /// Additionally we can introduce some preloading of data from database.
    pub cache: ParallelCacheState,
    /// Optional database that we use to fetch data from. If database is not present, we will
    /// return not existing account and storage.
    ///
    /// Note: It is marked as Send so database can be shared between threads.
    pub database: DB,
    /// Block state, it aggregates transactions transitions into one state.
    ///
    /// Build reverts and state that gets applied to the state.
    pub transition_state: Option<TransitionState>,
    /// After block is finishes we merge those changes inside bundle.
    /// Bundle is used to update database and create changesets.
    /// Bundle state can be set on initialization if we want to use preloaded bundle.
    pub bundle_state: BundleState,
    /// If EVM asks for block hash we will first check if they are found here.
    /// and then ask the database.
    ///
    /// This map can be used to give different values for block hashes if in case
    /// The fork block is different or some blocks are not saved inside database.
    pub block_hashes: DashMap<u64, B256, BuildIdentityHasher>,

    update_db_metrics: bool,
    db_latency: metrics::Histogram,
}

impl<DB> std::fmt::Debug for ParallelState<DB> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParallelState")
            .field("cache", &self.cache)
            .field("transition_state", &self.transition_state)
            .finish()
    }
}

impl<DB: DatabaseRef> ParallelState<DB> {
    /// Create a ParallelState
    /// #Parameters
    /// - `database`: the inner database to read the data not in cache
    /// - `with_bundle_update`: whether to update the bundle states
    /// - `update_db_metrics`: whether to report the database latency metrics
    pub fn new(database: DB, with_bundle_update: bool, update_db_metrics: bool) -> Self {
        Self {
            cache: ParallelCacheState::default(),
            database,
            transition_state: with_bundle_update.then(TransitionState::default),
            bundle_state: BundleState::default(),
            block_hashes: DashMap::default(),
            update_db_metrics,
            db_latency: histogram!("grevm.db_latency_us"),
        }
    }

    fn with_metrics<F, R>(&self, func: F) -> R
    where
        F: FnOnce() -> R,
    {
        if self.update_db_metrics {
            let start = Instant::now();
            let result = func();
            let duration = start.elapsed().as_nanos();
            self.db_latency.record(duration as f64);
            result
        } else {
            func()
        }
    }

    /// Returns the size hint for the inner bundle state.
    /// See [BundleState::size_hint] for more info.
    pub fn bundle_size_hint(&self) -> usize {
        self.bundle_state.size_hint()
    }

    /// Iterate over received balances and increment all account balances.
    /// If account is not found inside cache state it will be loaded from database.
    ///
    /// Update will create transitions for all accounts that are updated.
    ///
    /// Like [CacheAccount::increment_balance], this assumes that incremented balances are not
    /// zero, and will not overflow once incremented. If using this to implement withdrawals, zero
    /// balances must be filtered out before calling this function.
    pub fn increment_balances(
        &mut self,
        balances: impl IntoIterator<Item = (Address, u128)>,
    ) -> Result<(), DB::Error> {
        // make transition and update cache state
        let mut transitions = Vec::new();
        for (address, balance) in balances {
            if balance == 0 {
                continue;
            }
            let mut original_account = self.load_mut_cache_account(address)?;
            transitions.push((
                address,
                original_account.increment_balance(balance).expect("Balance is not zero"),
            ))
        }
        // append transition
        if let Some(s) = self.transition_state.as_mut() {
            s.add_transitions(transitions)
        }
        Ok(())
    }

    /// Drain balances from given account and return those values.
    ///
    /// It is used for DAO hardfork state change to move values from given accounts.
    pub fn drain_balances(
        &mut self,
        addresses: impl IntoIterator<Item = Address>,
    ) -> Result<Vec<u128>, DB::Error> {
        // make transition and update cache state
        let mut transitions = Vec::new();
        let mut balances = Vec::new();
        for address in addresses {
            let mut original_account = self.load_mut_cache_account(address)?;
            let (balance, transition) = original_account.drain_balance();
            balances.push(balance);
            transitions.push((address, transition))
        }
        // append transition
        if let Some(s) = self.transition_state.as_mut() {
            s.add_transitions(transitions)
        }
        Ok(balances)
    }

    /// State clear EIP-161 is enabled in Spurious Dragon hardfork.
    pub fn set_state_clear_flag(&mut self, has_state_clear: bool) {
        self.cache.set_state_clear_flag(has_state_clear);
    }

    /// Insert non-existent account
    pub fn insert_not_existing(&self, address: Address) {
        self.cache.insert_not_existing(address)
    }

    /// Insert account with specified `AccountInfo`
    pub fn insert_account(&self, address: Address, info: AccountInfo) {
        self.cache.insert_account(address, info)
    }

    /// Insert account with `AccountInfo` and `PlainStorage`
    pub fn insert_account_with_storage(
        &self,
        address: Address,
        info: AccountInfo,
        storage: PlainStorage,
    ) {
        self.cache.insert_account_with_storage(address, info, storage)
    }

    /// Apply evm transitions to transition state.
    pub fn apply_transition(&mut self, transitions: Vec<(Address, TransitionAccount)>) {
        // add transition to transition state.
        if let Some(s) = self.transition_state.as_mut() {
            s.add_transitions(transitions)
        }
    }

    /// Take all transitions and merge them inside bundle state.
    /// This action will create final post state and all reverts so that
    /// we at any time revert state of bundle to the state before transition
    /// is applied.
    pub fn merge_transitions(&mut self, retention: BundleRetention) {
        if let Some(transition_state) = self.transition_state.as_mut().map(TransitionState::take) {
            self.bundle_state.apply_transitions_and_create_reverts(transition_state, retention);
        }
    }

    /// Get a mutable reference to the [`CacheAccount`] for the given address.
    /// If the account is not found in the cache, it will be loaded from the
    /// database and inserted into the cache.
    pub fn load_mut_cache_account(
        &self,
        address: Address,
    ) -> Result<RefMut<'_, Address, CacheAccountInfo>, DB::Error> {
        if let Some(account) = self.cache.accounts.get_mut(&address) {
            return Ok(account);
        }
        let info = self.with_metrics(|| self.database.basic_ref(address))?;
        let account = match info {
            None => CacheAccountInfo::new(None, AccountStatus::LoadedNotExisting),
            Some(acc) if acc.is_empty() => CacheAccountInfo::new(
                Some(AccountInfo::default()),
                AccountStatus::LoadedEmptyEIP161,
            ),
            Some(acc) => CacheAccountInfo::new(Some(acc), AccountStatus::Loaded),
        };
        match self.cache.accounts.entry(address) {
            Entry::Vacant(entry) => Ok(entry.insert(account)),
            Entry::Occupied(entry) => Ok(entry.into_ref()),
        }
    }

    // TODO make cache aware of transitions dropping by having global transition counter.
    /// Takes the [`BundleState`] changeset from the [`State`], replacing it
    /// with an empty one.
    ///
    /// This will not apply any pending [`TransitionState`]. It is recommended
    /// to call [`State::merge_transitions`] before taking the bundle.
    ///
    /// If the `State` has been built with the
    /// [`StateBuilder::with_bundle_prestate`] option, the pre-state will be
    /// taken along with any changes made by [`State::merge_transitions`].
    pub fn take_bundle(&mut self) -> BundleState {
        core::mem::take(&mut self.bundle_state)
    }

    // Database stuff
    fn db_basic(&self, address: Address) -> Result<Option<AccountInfo>, DB::Error> {
        if let Some(account) = self.cache.accounts.get(&address) {
            return Ok(account.account.clone());
        }
        let info = self.with_metrics(|| self.database.basic_ref(address))?;
        let account = match info {
            None => CacheAccountInfo::new(None, AccountStatus::LoadedNotExisting),
            Some(acc) if acc.is_empty() => CacheAccountInfo::new(
                Some(AccountInfo::default()),
                AccountStatus::LoadedEmptyEIP161,
            ),
            Some(acc) => CacheAccountInfo::new(Some(acc), AccountStatus::Loaded),
        };
        match self.cache.accounts.entry(address) {
            Entry::Vacant(entry) => Ok(entry.insert(account).account.clone()),
            Entry::Occupied(entry) => Ok(entry.into_ref().account.clone()),
        }
    }

    fn db_code_by_hash(&self, code_hash: B256) -> Result<Bytecode, DB::Error> {
        if let Some(code) = self.cache.contracts.get(&code_hash) {
            return Ok(code.value().clone());
        }
        let code = self.with_metrics(|| self.database.code_by_hash_ref(code_hash))?;
        match self.cache.contracts.entry(code_hash) {
            Entry::Occupied(entry) => Ok(entry.get().clone()),
            Entry::Vacant(entry) => {
                // if not found in bundle ask database
                entry.insert(code.clone());
                Ok(code)
            }
        }
    }

    fn db_storage(&self, address: Address, index: U256) -> Result<U256, DB::Error> {
        if let Some(slots) = self.cache.storage.get(&address) {
            if let Some(value) = slots.get(&index) {
                return Ok(value.value().clone());
            }
        }
        // Account is guaranteed to be loaded.
        // Note that storage from bundle is already loaded with account.
        let is_storage_known = if let Some(account) = self.cache.accounts.get(&address) {
            // account will always be some, but if it is not, U256::ZERO will be returned.
            account.status.is_storage_known() || account.account.is_none()
        } else {
            unreachable!("For accessing any storage account is guaranteed to be loaded beforehand")
        };

        // if account was destroyed or account is newly built
        // we return zero and don't ask database.
        let value = if is_storage_known {
            U256::ZERO
        } else {
            self.with_metrics(|| self.database.storage_ref(address, index))?
        };
        let value = if let Some(slots) = self.cache.storage.get(&address) {
            slots.entry(index).or_insert(value).value().clone()
        } else {
            match self.cache.storage.entry(address) {
                Entry::Occupied(entry) => entry.get().entry(index).or_insert(value).value().clone(),
                Entry::Vacant(entry) => {
                    entry.insert(Default::default()).entry(index).or_insert(value).value().clone()
                }
            }
        };
        Ok(value)
    }

    fn db_block_hash(&self, number: u64) -> Result<B256, DB::Error> {
        match self.block_hashes.entry(number) {
            Entry::Occupied(entry) => Ok(*entry.get()),
            Entry::Vacant(entry) => {
                let ret =
                    *entry.insert(self.with_metrics(|| self.database.block_hash_ref(number))?);
                Ok(ret)
            }
        }
    }
}

impl<DB: DatabaseRef> Database for ParallelState<DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.db_basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db_code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.db_storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db_block_hash(number)
    }
}

impl<DB: DatabaseRef> DatabaseRef for ParallelState<DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.db_basic(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db_code_by_hash(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.db_storage(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.db_block_hash(number)
    }
}

impl<DB: DatabaseRef> DatabaseCommit for ParallelState<DB> {
    fn commit(&mut self, evm_state: HashMap<Address, Account>) {
        let transitions = self.cache.apply_evm_state(evm_state);
        self.apply_transition(transitions);
    }
}
