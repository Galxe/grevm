use crate::{
    storage::{CachedStorageData, ParallelBundleState},
    LocationAndType, MemoryValue,
};
use revm::{
    db::{
        states::{bundle_state::BundleRetention, plain_account::PlainStorage, CacheAccount},
        BundleState,
    },
    State, StateBuilder, TransitionState,
};
use revm_primitives::{
    db::{DatabaseCommit, DatabaseRef, WrapDatabaseRef},
    AccountInfo, Address, Bytecode, ExecutionResult, ResultAndState, B256, U256,
};
use std::collections::hash_map;

pub struct StateAsyncCommit<DB>
where
    DB: DatabaseRef,
{
    coinbase: Address,
    pub results: Vec<ExecutionResult>,
    pub state: State<WrapDatabaseRef<DB>>,
}

impl<DB> StateAsyncCommit<DB>
where
    DB: DatabaseRef,
{
    pub fn new(coinbase: Address, db: DB) -> Self {
        let state = StateBuilder::new().with_bundle_update().with_database_ref(db).build();
        Self { coinbase, results: vec![], state }
    }

    pub fn init(&mut self) -> Result<(), DB::Error> {
        match self.state.load_cache_account(self.coinbase) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn take_result(&mut self) -> Vec<ExecutionResult> {
        std::mem::take(&mut self.results)
    }

    pub fn take_bundle(&mut self) -> BundleState {
        if let Some(transition_state) =
            self.state.transition_state.as_mut().map(TransitionState::take)
        {
            self.state.bundle_state.parallel_apply_transitions_and_create_reverts(
                transition_state,
                BundleRetention::Reverts,
            );
        }

        self.state.take_bundle()
    }

    pub fn commit(
        &mut self,
        mut result_and_state: ResultAndState,
        read_set: Vec<LocationAndType>,
        cache: &CachedStorageData,
    ) {
        self.apply_cache(read_set, cache);
        let ResultAndState { result, state, rewards } = result_and_state;
        self.results.push(result);
        self.state.commit(state);
        assert!(self.state.increment_balances(vec![(self.coinbase, rewards)]).is_ok());
    }

    fn account_info(&self, address: Address) -> Option<AccountInfo> {
        self.state.cache.accounts.get(&address).and_then(|account| account.account_info())
    }

    fn contains_account(&self, address: Address) -> bool {
        self.state.cache.accounts.contains_key(&address)
    }

    fn insert_account(&mut self, address: Address, info: Option<AccountInfo>) {
        let account = match info {
            None => CacheAccount::new_loaded_not_existing(),
            Some(acc) if acc.is_empty() => {
                CacheAccount::new_loaded_empty_eip161(PlainStorage::new())
            }
            Some(acc) => CacheAccount::new_loaded(acc, PlainStorage::new()),
        };
        self.state.cache.accounts.insert(address, account);
    }

    fn contains_code(&self, code_hash: B256) -> bool {
        self.state.cache.contracts.contains_key(&code_hash)
    }

    fn insert_code(&mut self, code_hash: B256, code: Bytecode) {
        self.state.cache.contracts.insert(code_hash, code);
    }

    fn contains_storage_slot(&self, address: Address, index: U256) -> bool {
        match self.state.cache.accounts.get(&address) {
            Some(cache_account) => match &cache_account.account {
                Some(account) => account.storage.contains_key(&index),
                None => false,
            },
            None => false,
        }
    }

    fn insert_storage_slot(&mut self, address: Address, index: U256, value: U256) {
        match self.state.cache.accounts.entry(address) {
            hash_map::Entry::Vacant(_) => {
                panic!("Unreachable");
            }
            hash_map::Entry::Occupied(entry) => {
                if let Some(account) = &mut entry.into_mut().account {
                    account.storage.insert(index, value);
                } else {
                    panic!("Unreachable");
                }
            }
        }
    }

    fn apply_cache(&mut self, read_set: Vec<LocationAndType>, cache: &CachedStorageData) {
        for location in read_set.into_iter() {
            match location {
                LocationAndType::Basic(address) => {
                    if !self.contains_account(address) {
                        if let Some(info) = cache.get(&LocationAndType::Basic(address)) {
                            if let MemoryValue::Basic(info) = info.value() {
                                self.insert_account(address, Some(info.clone()));
                            }
                        } else {
                            self.insert_account(address, None);
                        }
                    }
                }
                LocationAndType::Storage(address, index) => {
                    if !self.contains_storage_slot(address, index) {
                        if let Some(slot) = cache.get(&LocationAndType::Storage(address, index)) {
                            if let MemoryValue::Storage(slot) = slot.value() {
                                self.insert_storage_slot(address, index, slot.clone());
                            }
                        }
                    }
                }
                LocationAndType::Code(address) => {
                    if let Some(info) = self.account_info(address) {
                        if !info.is_empty_code_hash() {
                            let code_hash = info.code_hash;
                            if !self.contains_code(code_hash) {
                                if let Some(code) = cache.get(&LocationAndType::CodeHash(code_hash))
                                {
                                    if let MemoryValue::CodeHash(code) = code.value() {
                                        self.insert_code(code_hash, code.clone());
                                    }
                                }
                            }
                        }
                    }
                }
                LocationAndType::CodeHash(_) => {
                    panic!("Unreachable");
                }
            }
        }
    }
}
