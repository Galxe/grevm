use core::hash::{BuildHasherDefault, Hasher};
use dashmap::{mapref::one::RefMut, DashMap, Entry};
use revm::{
    db::{
        states::{bundle_state::BundleRetention, plain_account::PlainStorage, CacheAccount},
        BundleState, PlainAccount,
    },
    TransitionAccount, TransitionState,
};
use revm_primitives::{
    db::{Database, DatabaseCommit, DatabaseRef},
    Account, AccountInfo, Address, Bytecode, EvmState, B256, U256,
};
use std::{
    collections::{hash_map, HashMap},
    vec::Vec,
};

/// We use the last 8 bytes of an existing hash like address
/// or code hash instead of rehashing it.
// TODO: Make sure this is acceptable for production
#[derive(Debug, Default)]
pub struct SuffixHasher(u64);
impl Hasher for SuffixHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut suffix = [0u8; 8];
        suffix.copy_from_slice(&bytes[bytes.len() - 8..]);
        self.0 = u64::from_be_bytes(suffix);
    }
}
/// Build a suffix hasher
pub type BuildSuffixHasher = BuildHasherDefault<SuffixHasher>;

/// Cache state contains both modified and original values.
///
/// Cache state is main state that revm uses to access state.
/// It loads all accounts from database and applies revm output to it.
///
/// It generates transitions that is used to build BundleState.
#[derive(Clone, Debug)]
pub struct ParallelCacheState {
    /// Block state account with account state.
    pub accounts: DashMap<Address, CacheAccount, BuildSuffixHasher>,
    /// Created contracts.
    // TODO add bytecode counter for number of bytecodes added/removed.
    pub contracts: DashMap<B256, Bytecode, BuildSuffixHasher>,
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
        Self { accounts: DashMap::default(), contracts: DashMap::default(), has_state_clear }
    }

    /// Set state clear flag. EIP-161.
    pub fn set_state_clear_flag(&mut self, has_state_clear: bool) {
        self.has_state_clear = has_state_clear;
    }

    /// Helper function that returns all accounts.
    ///
    /// Used inside tests to generate merkle tree.
    pub fn trie_account(&self) -> impl IntoIterator<Item = (Address, PlainAccount)> + '_ {
        self.accounts.iter().filter_map(|r| {
            r.value().account.as_ref().map(|plain_acc| (*r.key(), plain_acc.clone()))
        })
    }

    /// Insert not existing account.
    pub fn insert_not_existing(&mut self, address: Address) {
        self.accounts.insert(address, CacheAccount::new_loaded_not_existing());
    }

    /// Insert Loaded (Or LoadedEmptyEip161 if account is empty) account.
    pub fn insert_account(&mut self, address: Address, info: AccountInfo) {
        let account = if !info.is_empty() {
            CacheAccount::new_loaded(info, HashMap::default())
        } else {
            CacheAccount::new_loaded_empty_eip161(HashMap::default())
        };
        self.accounts.insert(address, account);
    }

    /// Similar to `insert_account` but with storage.
    pub fn insert_account_with_storage(
        &mut self,
        address: Address,
        info: AccountInfo,
        storage: PlainStorage,
    ) {
        let account = if !info.is_empty() {
            CacheAccount::new_loaded(info, storage)
        } else {
            CacheAccount::new_loaded_empty_eip161(storage)
        };
        self.accounts.insert(address, account);
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

    /// Apply updated account state to the cached account.
    /// Returns account transition if applicable.
    fn apply_account_state(&self, address: Address, account: Account) -> Option<TransitionAccount> {
        // not touched account are never changed.
        if !account.is_touched() {
            return None;
        }

        let mut this_account =
            self.accounts.get_mut(&address).expect("All accounts should be present inside cache");

        // If it is marked as selfdestructed inside revm
        // we need to changed state to destroyed.
        if account.is_selfdestructed() {
            return this_account.selfdestruct();
        }

        let is_created = account.is_created();
        let is_empty = account.is_empty();

        // transform evm storage to storage with previous value.
        let changed_storage = account
            .storage
            .into_iter()
            .filter(|(_, slot)| slot.is_changed())
            .map(|(key, slot)| (key, slot.into()))
            .collect();

        // Note: it can happen that created contract get selfdestructed in same block
        // that is why is_created is checked after selfdestructed
        //
        // Note: Create2 opcode (Petersburg) was after state clear EIP (Spurious Dragon)
        //
        // Note: It is possibility to create KECCAK_EMPTY contract with some storage
        // by just setting storage inside CRATE constructor. Overlap of those contracts
        // is not possible because CREATE2 is introduced later.
        if is_created {
            self.contracts
                .entry(account.info.code_hash)
                .or_insert_with(|| account.info.code.clone().unwrap());
            return Some(this_account.newly_created(account.info, changed_storage));
        }

        // Account is touched, but not selfdestructed or newly created.
        // Account can be touched and not changed.
        // And when empty account is touched it needs to be removed from database.
        // EIP-161 state clear
        if is_empty {
            if self.has_state_clear {
                // touch empty account.
                this_account.touch_empty_eip161()
            } else {
                // if account is empty and state clear is not enabled we should save
                // empty account.
                this_account.touch_create_pre_eip161(changed_storage)
            }
        } else {
            Some(this_account.change(account.info, changed_storage))
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
pub type BuildIdentityHasher = BuildHasherDefault<IdentityHasher>;

/// State of blockchain.
///
/// State clear flag is set inside CacheState and by default it is enabled.
/// If you want to disable it use `set_state_clear_flag` function.
#[derive(Debug)]
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
    /// Addition layer that is going to be used to fetched values before fetching values
    /// from database.
    ///
    /// Bundle is the main output of the state execution and this allows setting previous bundle
    /// and using its values for execution.
    pub use_preloaded_bundle: bool,
    /// If EVM asks for block hash we will first check if they are found here.
    /// and then ask the database.
    ///
    /// This map can be used to give different values for block hashes if in case
    /// The fork block is different or some blocks are not saved inside database.
    pub block_hashes: DashMap<u64, B256, BuildIdentityHasher>,
}

impl<DB: DatabaseRef> ParallelState<DB> {
    pub fn new(database: DB, with_bundle_update: bool) -> Self {
        Self {
            cache: ParallelCacheState::default(),
            database,
            transition_state: with_bundle_update.then(TransitionState::default),
            bundle_state: BundleState::default(),
            use_preloaded_bundle: false,
            block_hashes: DashMap::default(),
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
            let mut original_account = self.load_cache_account(address)?;
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
            let mut original_account = self.load_cache_account(address)?;
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

    pub fn insert_not_existing(&mut self, address: Address) {
        self.cache.insert_not_existing(address)
    }

    pub fn insert_account(&mut self, address: Address, info: AccountInfo) {
        self.cache.insert_account(address, info)
    }

    pub fn insert_account_with_storage(
        &mut self,
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
    pub fn load_cache_account(
        &self,
        address: Address,
    ) -> Result<RefMut<'_, Address, CacheAccount>, DB::Error> {
        match self.cache.accounts.entry(address) {
            Entry::Vacant(entry) => {
                if self.use_preloaded_bundle {
                    // load account from bundle state
                    if let Some(account) =
                        self.bundle_state.account(&address).cloned().map(Into::into)
                    {
                        return Ok(entry.insert(account));
                    }
                }
                // if not found in bundle, load it from database
                let info = self.database.basic_ref(address)?;
                let account = match info {
                    None => CacheAccount::new_loaded_not_existing(),
                    Some(acc) if acc.is_empty() => {
                        CacheAccount::new_loaded_empty_eip161(HashMap::default())
                    }
                    Some(acc) => CacheAccount::new_loaded(acc, HashMap::default()),
                };
                Ok(entry.insert(account))
            }
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
    //
    fn db_basic(&self, address: Address) -> Result<Option<AccountInfo>, DB::Error> {
        self.load_cache_account(address).map(|a| a.account_info())
    }

    fn db_code_by_hash(&self, code_hash: B256) -> Result<Bytecode, DB::Error> {
        let res = match self.cache.contracts.entry(code_hash) {
            Entry::Occupied(entry) => Ok(entry.get().clone()),
            Entry::Vacant(entry) => {
                if self.use_preloaded_bundle {
                    if let Some(code) = self.bundle_state.contracts.get(&code_hash) {
                        entry.insert(code.clone());
                        return Ok(code.clone());
                    }
                }
                // if not found in bundle ask database
                let code = self.database.code_by_hash_ref(code_hash)?;
                entry.insert(code.clone());
                Ok(code)
            }
        };
        res
    }

    fn db_storage(&self, address: Address, index: U256) -> Result<U256, DB::Error> {
        // Account is guaranteed to be loaded.
        // Note that storage from bundle is already loaded with account.
        if let Some(mut account) = self.cache.accounts.get_mut(&address) {
            // account will always be some, but if it is not, U256::ZERO will be returned.
            let is_storage_known = account.status.is_storage_known();
            Ok(account
                .account
                .as_mut()
                .map(|account| match account.storage.entry(index) {
                    hash_map::Entry::Occupied(entry) => Ok(*entry.get()),
                    hash_map::Entry::Vacant(entry) => {
                        // if account was destroyed or account is newly built
                        // we return zero and don't ask database.
                        let value = if is_storage_known {
                            U256::ZERO
                        } else {
                            self.database.storage_ref(address, index)?
                        };
                        entry.insert(value);
                        Ok(value)
                    }
                })
                .transpose()?
                .unwrap_or_default())
        } else {
            unreachable!("For accessing any storage account is guaranteed to be loaded beforehand")
        }
    }

    fn db_block_hash(&self, number: u64) -> Result<B256, DB::Error> {
        match self.block_hashes.entry(number) {
            Entry::Occupied(entry) => Ok(*entry.get()),
            Entry::Vacant(entry) => {
                let ret = *entry.insert(self.database.block_hash_ref(number)?);
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
