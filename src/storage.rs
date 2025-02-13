use crate::{
    fork_join_util, scheduler::MVMemory, AccountBasic, LocationAndType, MemoryEntry, MemoryValue,
    ParallelState, ReadVersion, TxId, TxVersion,
};
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use parking_lot::Mutex;
use revm::{
    db::{states::bundle_state::BundleRetention, AccountRevert, BundleAccount, BundleState},
    interpreter::analysis::to_analysed,
    TransitionState,
};
use revm_primitives::{
    db::{Database, DatabaseRef},
    AccountInfo, Address, Bytecode, EvmState, B256, U256,
};
use std::sync::atomic::{AtomicUsize, Ordering};

pub trait ParallelBundleState {
    fn parallel_apply_transitions_and_create_reverts(
        &mut self,
        transitions: TransitionState,
        retention: BundleRetention,
    );
}

impl ParallelBundleState for BundleState {
    fn parallel_apply_transitions_and_create_reverts(
        &mut self,
        transitions: TransitionState,
        retention: BundleRetention,
    ) {
        if !self.state.is_empty() {
            self.apply_transitions_and_create_reverts(transitions, retention);
            return;
        }

        let include_reverts = retention.includes_reverts();
        // pessimistically pre-allocate assuming _all_ accounts changed.
        let reverts_capacity = if include_reverts { transitions.transitions.len() } else { 0 };
        let transitions = transitions.transitions;
        let addresses: Vec<Address> = transitions.keys().cloned().collect();
        let reverts: Vec<Option<(Address, AccountRevert)>> = vec![None; reverts_capacity];
        let bundle_state: Vec<Option<(Address, BundleAccount)>> = vec![None; transitions.len()];
        let state_size = AtomicUsize::new(0);
        let contracts = Mutex::new(std::collections::HashMap::new());

        fork_join_util(transitions.len(), None, |start_pos, end_pos, _| {
            #[allow(invalid_reference_casting)]
            let reverts = unsafe {
                &mut *(&reverts as *const Vec<Option<(Address, AccountRevert)>>
                    as *mut Vec<Option<(Address, AccountRevert)>>)
            };
            #[allow(invalid_reference_casting)]
            let addresses =
                unsafe { &mut *(&addresses as *const Vec<Address> as *mut Vec<Address>) };
            #[allow(invalid_reference_casting)]
            let bundle_state = unsafe {
                &mut *(&bundle_state as *const Vec<Option<(Address, BundleAccount)>>
                    as *mut Vec<Option<(Address, BundleAccount)>>)
            };

            for pos in start_pos..end_pos {
                let address = addresses[pos];
                let transition = transitions.get(&address).cloned().unwrap();
                // add new contract if it was created/changed.
                if let Some((hash, new_bytecode)) = transition.has_new_contract() {
                    contracts.lock().insert(hash, new_bytecode.clone());
                }
                let present_bundle = transition.present_bundle_account();
                let revert = transition.create_revert();
                if let Some(revert) = revert {
                    state_size.fetch_add(present_bundle.size_hint(), Ordering::Relaxed);
                    bundle_state[pos] = Some((address, present_bundle));
                    if include_reverts {
                        reverts[pos] = Some((address, revert));
                    }
                }
            }
        });
        self.state_size = state_size.load(Ordering::Acquire);

        // much faster than bundle_state.into_iter().filter_map(|r| r).collect()
        self.state.reserve(transitions.len());
        for bundle in bundle_state {
            if let Some((address, state)) = bundle {
                self.state.insert(address, state);
            }
        }
        let mut final_reverts = Vec::with_capacity(reverts_capacity);
        for revert in reverts {
            if let Some(r) = revert {
                final_reverts.push(r);
            }
        }
        self.reverts.push(final_reverts);
        self.contracts = contracts.into_inner();
    }
}

pub trait ParallelTakeBundle {
    fn parallel_take_bundle(&mut self, retention: BundleRetention) -> BundleState;
}

impl<DB: DatabaseRef> ParallelTakeBundle for ParallelState<DB> {
    fn parallel_take_bundle(&mut self, retention: BundleRetention) -> BundleState {
        if let Some(transition_state) = self.transition_state.as_mut().map(TransitionState::take) {
            self.bundle_state
                .parallel_apply_transitions_and_create_reverts(transition_state, retention);
        }
        self.take_bundle()
    }
}

pub(crate) struct CacheDB<'a, DB>
where
    DB: DatabaseRef,
{
    coinbase: Address,
    db: &'a DB,
    mv_memory: &'a MVMemory,
    num_commit: &'a AtomicUsize,

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
    pub fn new(
        coinbase: Address,
        db: &'a DB,
        mv_memory: &'a MVMemory,
        num_commit: &'a AtomicUsize,
    ) -> Self {
        Self {
            coinbase,
            db,
            mv_memory,
            num_commit,
            read_set: HashMap::new(),
            read_accounts: HashMap::new(),
            current_tx: TxVersion::new(0, 0),
            accurate_origin: true,
            estimate_txs: HashSet::new(),
        }
    }

    pub fn reset_state(&mut self, tx_version: TxVersion) {
        self.current_tx = tx_version;
        self.read_set.clear();
        self.read_accounts.clear();
        self.accurate_origin = true;
        self.estimate_txs.clear();
    }

    pub fn read_accurate_origin(&self) -> bool {
        self.accurate_origin
    }

    pub fn take_estimate_txs(&mut self) -> HashSet<TxId> {
        std::mem::take(&mut self.estimate_txs)
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
            if *address == self.coinbase {
                continue;
            }
            if account.is_selfdestructed() {
                let memory_entry = MemoryEntry::new(
                    self.current_tx.incarnation,
                    MemoryValue::SelfDestructed,
                    estimate,
                );
                write_set.insert(LocationAndType::Code(address.clone()));
                write_set.insert(LocationAndType::Basic(address.clone()));
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
                        if self.num_commit.load(Ordering::Relaxed) == self.current_tx.txid {
                            // make sure read after the latest self-destructed
                            read_version =
                                ReadVersion::MvMemory(TxVersion::new(txid, entry.incarnation));
                        } else {
                            self.accurate_origin = false;
                            result = Some(Bytecode::default());
                        }
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
}

impl<'a, DB> Database for CacheDB<'a, DB>
where
    DB: DatabaseRef,
{
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let mut result = None;
        if address == self.coinbase {
            self.accurate_origin = self.num_commit.load(Ordering::Relaxed) == self.current_tx.txid;
            result = self.db.basic_ref(address.clone())?;
        } else {
            let mut read_version = ReadVersion::Storage;
            let mut read_account = AccountBasic { balance: U256::ZERO, nonce: 0, code_hash: None };
            let location = LocationAndType::Basic(address.clone());
            // 1. read from multi-version memory
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
                            if self.num_commit.load(Ordering::Relaxed) == self.current_tx.txid {
                                // make sure read after the latest self-destructed
                                read_version =
                                    ReadVersion::MvMemory(TxVersion::new(txid, entry.incarnation));
                            } else {
                                self.accurate_origin = false;
                                result = Some(AccountInfo::default());
                            }
                        }
                        _ => {}
                    }
                }
            }
            // 2. read from database
            if result.is_none() {
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
                }
            }
            self.read_accounts.insert(address, read_account);
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
            let mut new_ca = false;
            if let Some(ReadVersion::MvMemory(_)) =
                self.read_set.get(&LocationAndType::Code(address.clone()))
            {
                new_ca = true;
            }
            let slot =
                if new_ca { U256::default() } else { self.db.storage_ref(address, index)? };
            result = Some(slot.clone());
        }

        self.read_set.insert(location, read_version);
        Ok(result.expect("No storage slot"))
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash_ref(number)
    }
}
