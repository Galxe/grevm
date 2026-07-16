use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use revm::context_interface::{
    context::{SStoreResult, SelfDestructResult},
    journaled_state::StateLoad,
};
use revm_context::{
    JournalTr,
    journaled_state::{AccountLoad, JournalCheckpoint, TransferError},
};
use revm_primitives::{
    Address, B256, HashSet as PrimitiveHashSet, Log, StorageKey, StorageValue, U256,
    hardfork::SpecId,
};
use revm_state::{Account, Bytecode};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TrackingPhase {
    #[default]
    Off,
    PreExecution,
    Authorization,
    Execution,
}

#[derive(Clone, Debug)]
enum Undo {
    SenderPreBalance(Option<U256>),
    SenderWasDelegated(bool),
    AuthorizationSensitive(Address, bool),
    DelegatedSubject(Address, bool),
    OriginalBalance(Address, Option<U256>),
    Debited(Address, bool),
    CreateTxSenderNonceBumped(bool),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ReserveTracker {
    phase: TrackingPhase,
    sender: Address,
    tx_kind_create: bool,
    sender_pre_tx_balance: Option<U256>,
    sender_was_delegated: bool,
    authorization_sensitive: HashSet<Address>,
    delegated_subjects: HashSet<Address>,
    original_balances: HashMap<Address, U256>,
    debited_accounts: HashSet<Address>,
    create_tx_sender_nonce_bumped: bool,
    undo: Vec<Undo>,
    checkpoints: Vec<usize>,
}

impl ReserveTracker {
    fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn begin_transaction(&mut self, sender: Address, tx_kind_create: bool) {
        self.clear();
        self.sender = sender;
        self.tx_kind_create = tx_kind_create;
    }

    pub(crate) fn start_pre_execution(&mut self) {
        self.phase = TrackingPhase::PreExecution;
    }

    pub(crate) fn stop_pre_execution(&mut self) {
        self.phase = TrackingPhase::Off;
    }

    pub(crate) fn start_authorization(&mut self) {
        self.phase = TrackingPhase::Authorization;
    }

    pub(crate) fn stop_authorization(&mut self) {
        self.phase = TrackingPhase::Off;
    }

    pub(crate) fn start_execution(&mut self) {
        self.phase = TrackingPhase::Execution;
    }

    pub(crate) fn stop_execution(&mut self) {
        self.phase = TrackingPhase::Off;
    }

    pub(crate) fn mark_authorization_sensitive(&mut self, address: Address) {
        self.insert_authorization_sensitive(address);
    }

    pub(crate) fn protected_debits(&self) -> Vec<Address> {
        self.debited_accounts
            .iter()
            .copied()
            .filter(|address| self.is_protected(*address))
            .collect()
    }

    pub(crate) fn original_balance(&self, address: Address) -> Option<U256> {
        if address == self.sender {
            self.sender_pre_tx_balance
        } else {
            self.original_balances.get(&address).copied()
        }
    }

    pub(crate) fn create_tx_sender_nonce_bumped(&self) -> bool {
        self.create_tx_sender_nonce_bumped
    }

    fn is_protected(&self, address: Address) -> bool {
        self.authorization_sensitive.contains(&address) ||
            self.delegated_subjects.contains(&address) ||
            (address == self.sender && self.sender_was_delegated)
    }

    fn set_sender_pre_tx_balance(&mut self, balance: U256) {
        if self.sender_pre_tx_balance.is_none() {
            self.undo.push(Undo::SenderPreBalance(self.sender_pre_tx_balance));
            self.sender_pre_tx_balance = Some(balance);
        }
    }

    fn set_sender_was_delegated(&mut self, delegated: bool) {
        if delegated && !self.sender_was_delegated {
            self.undo.push(Undo::SenderWasDelegated(false));
            self.sender_was_delegated = true;
        }
    }

    fn insert_authorization_sensitive(&mut self, address: Address) {
        let existed = self.authorization_sensitive.contains(&address);
        if !existed {
            self.undo.push(Undo::AuthorizationSensitive(address, false));
            self.authorization_sensitive.insert(address);
        }
    }

    fn insert_delegated_subject(&mut self, address: Address) {
        let existed = self.delegated_subjects.contains(&address);
        if !existed {
            self.undo.push(Undo::DelegatedSubject(address, false));
            self.delegated_subjects.insert(address);
        }
    }

    fn save_original_balance(&mut self, address: Address, balance: U256) {
        if !matches!(self.phase, TrackingPhase::Execution) {
            return;
        }
        if !self.original_balances.contains_key(&address) {
            self.undo.push(Undo::OriginalBalance(address, None));
            self.original_balances.insert(address, balance);
        }
    }

    fn record_debit(&mut self, address: Address) {
        if !matches!(self.phase, TrackingPhase::Execution) {
            return;
        }
        let existed = self.debited_accounts.contains(&address);
        if !existed {
            self.undo.push(Undo::Debited(address, false));
            self.debited_accounts.insert(address);
        }
    }

    fn record_create_tx_sender_nonce_bump(&mut self, address: Address) {
        if matches!(self.phase, TrackingPhase::Execution) &&
            self.tx_kind_create &&
            address == self.sender &&
            !self.create_tx_sender_nonce_bumped
        {
            self.undo.push(Undo::CreateTxSenderNonceBumped(self.create_tx_sender_nonce_bumped));
            self.create_tx_sender_nonce_bumped = true;
        }
    }

    fn checkpoint(&mut self) {
        self.checkpoints.push(self.undo.len());
    }

    fn checkpoint_commit(&mut self) {
        self.checkpoints.pop();
    }

    fn checkpoint_revert(&mut self) {
        let Some(undo_len) = self.checkpoints.pop() else {
            return;
        };
        while self.undo.len() > undo_len {
            match self.undo.pop().unwrap() {
                Undo::SenderPreBalance(value) => self.sender_pre_tx_balance = value,
                Undo::SenderWasDelegated(value) => self.sender_was_delegated = value,
                Undo::AuthorizationSensitive(address, existed) => {
                    if existed {
                        self.authorization_sensitive.insert(address);
                    } else {
                        self.authorization_sensitive.remove(&address);
                    }
                }
                Undo::DelegatedSubject(address, existed) => {
                    if existed {
                        self.delegated_subjects.insert(address);
                    } else {
                        self.delegated_subjects.remove(&address);
                    }
                }
                Undo::OriginalBalance(address, previous) => {
                    if let Some(balance) = previous {
                        self.original_balances.insert(address, balance);
                    } else {
                        self.original_balances.remove(&address);
                    }
                }
                Undo::Debited(address, existed) => {
                    if existed {
                        self.debited_accounts.insert(address);
                    } else {
                        self.debited_accounts.remove(&address);
                    }
                }
                Undo::CreateTxSenderNonceBumped(value) => {
                    self.create_tx_sender_nonce_bumped = value;
                }
            }
        }
    }
}

pub(crate) trait TrackingJournalExt {
    fn tracker(&self) -> &ReserveTracker;
    fn tracker_mut(&mut self) -> &mut ReserveTracker;
}

#[derive(Clone, Debug)]
pub(crate) struct TrackingJournal<J> {
    inner: J,
    tracker: ReserveTracker,
}

impl<J: JournalTr> JournalTr for TrackingJournal<J> {
    type Database = J::Database;
    type State = J::State;

    fn new(database: Self::Database) -> Self {
        Self { inner: J::new(database), tracker: ReserveTracker::default() }
    }

    fn db_mut(&mut self) -> &mut Self::Database {
        self.inner.db_mut()
    }

    fn db(&self) -> &Self::Database {
        self.inner.db()
    }

    fn sload(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<StateLoad<StorageValue>, <Self::Database as revm::Database>::Error> {
        self.inner.sload(address, key)
    }

    fn sstore(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, <Self::Database as revm::Database>::Error> {
        self.inner.sstore(address, key, value)
    }

    fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue {
        self.inner.tload(address, key)
    }

    fn tstore(&mut self, address: Address, key: StorageKey, value: StorageValue) {
        self.inner.tstore(address, key, value)
    }

    fn log(&mut self, log: Log) {
        self.inner.log(log)
    }

    fn selfdestruct(
        &mut self,
        address: Address,
        target: Address,
    ) -> Result<StateLoad<SelfDestructResult>, <Self::Database as revm::Database>::Error> {
        let address_balance = self.inner.load_account(address)?.data.info.balance;
        let target_balance = if address == target {
            address_balance
        } else {
            self.inner.load_account(target)?.data.info.balance
        };

        let result = self.inner.selfdestruct(address, target)?;
        if !address_balance.is_zero() {
            self.tracker.save_original_balance(address, address_balance);
            self.tracker.record_debit(address);
            if address != target {
                self.tracker.save_original_balance(target, target_balance);
            }
        }
        Ok(result)
    }

    fn warm_account_and_storage(
        &mut self,
        address: Address,
        storage_keys: impl IntoIterator<Item = StorageKey>,
    ) -> Result<(), <Self::Database as revm::Database>::Error> {
        self.inner.warm_account_and_storage(address, storage_keys)
    }

    fn warm_coinbase_account(&mut self, address: Address) {
        self.inner.warm_coinbase_account(address)
    }

    fn warm_precompiles(&mut self, addresses: PrimitiveHashSet<Address>) {
        self.inner.warm_precompiles(addresses)
    }

    fn precompile_addresses(&self) -> &PrimitiveHashSet<Address> {
        self.inner.precompile_addresses()
    }

    fn set_spec_id(&mut self, spec_id: SpecId) {
        self.inner.set_spec_id(spec_id)
    }

    fn touch_account(&mut self, address: Address) {
        self.inner.touch_account(address)
    }

    fn transfer(
        &mut self,
        from: Address,
        to: Address,
        balance: U256,
    ) -> Result<Option<TransferError>, <Self::Database as revm::Database>::Error> {
        if balance.is_zero() {
            return self.inner.transfer(from, to, balance);
        }

        let from_balance = self.inner.load_account(from)?.data.info.balance;
        let to_balance =
            if from == to { from_balance } else { self.inner.load_account(to)?.data.info.balance };

        let result = self.inner.transfer(from, to, balance)?;
        if result.is_none() {
            self.tracker.save_original_balance(from, from_balance);
            self.tracker.record_debit(from);
            if from != to {
                self.tracker.save_original_balance(to, to_balance);
            }
        }
        Ok(result)
    }

    fn caller_accounting_journal_entry(
        &mut self,
        address: Address,
        old_balance: U256,
        bump_nonce: bool,
    ) {
        if matches!(self.tracker.phase, TrackingPhase::PreExecution) &&
            address == self.tracker.sender
        {
            self.tracker.set_sender_pre_tx_balance(old_balance);
        }
        self.inner.caller_accounting_journal_entry(address, old_balance, bump_nonce)
    }

    fn balance_incr(
        &mut self,
        address: Address,
        balance: U256,
    ) -> Result<(), <Self::Database as revm::Database>::Error> {
        if balance.is_zero() || !matches!(self.tracker.phase, TrackingPhase::Execution) {
            return self.inner.balance_incr(address, balance);
        }
        let old_balance = self.inner.load_account(address)?.data.info.balance;
        let result = self.inner.balance_incr(address, balance);
        if result.is_ok() {
            self.tracker.save_original_balance(address, old_balance);
        }
        result
    }

    fn nonce_bump_journal_entry(&mut self, address: Address) {
        self.tracker.record_create_tx_sender_nonce_bump(address);
        self.inner.nonce_bump_journal_entry(address)
    }

    fn load_account(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, <Self::Database as revm::Database>::Error> {
        self.inner.load_account(address)
    }

    fn load_account_code(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, <Self::Database as revm::Database>::Error> {
        let load = self.inner.load_account_code(address)?;
        if matches!(self.tracker.phase, TrackingPhase::PreExecution) &&
            address == self.tracker.sender
        {
            let delegated = load.data.info.code.as_ref().is_some_and(Bytecode::is_eip7702);
            self.tracker.set_sender_was_delegated(delegated);
        }
        Ok(load)
    }

    fn load_account_delegated(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<AccountLoad>, <Self::Database as revm::Database>::Error> {
        let load = self.inner.load_account_delegated(address)?;
        if load.is_delegate_account_cold.is_some() {
            self.tracker.insert_delegated_subject(address);
        }
        Ok(load)
    }

    fn set_code_with_hash(&mut self, address: Address, code: Bytecode, hash: B256) {
        if matches!(self.tracker.phase, TrackingPhase::Authorization) {
            self.tracker.insert_authorization_sensitive(address);
        }
        self.inner.set_code_with_hash(address, code, hash)
    }

    fn checkpoint(&mut self) -> JournalCheckpoint {
        self.tracker.checkpoint();
        self.inner.checkpoint()
    }

    fn checkpoint_commit(&mut self) {
        self.tracker.checkpoint_commit();
        self.inner.checkpoint_commit()
    }

    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        self.tracker.checkpoint_revert();
        self.inner.checkpoint_revert(checkpoint)
    }

    fn create_account_checkpoint(
        &mut self,
        caller: Address,
        address: Address,
        balance: U256,
        spec_id: SpecId,
    ) -> Result<JournalCheckpoint, TransferError> {
        let caller_balance = self
            .inner
            .load_account(caller)
            .map_err(|_| TransferError::OutOfFunds)?
            .data
            .info
            .balance;
        let target_balance = self
            .inner
            .load_account(address)
            .map_err(|_| TransferError::OutOfFunds)?
            .data
            .info
            .balance;
        self.tracker.checkpoint();
        match self.inner.create_account_checkpoint(caller, address, balance, spec_id) {
            Ok(checkpoint) => {
                if !balance.is_zero() {
                    self.tracker.save_original_balance(caller, caller_balance);
                    self.tracker.record_debit(caller);
                    self.tracker.save_original_balance(address, target_balance);
                }
                Ok(checkpoint)
            }
            Err(error) => {
                self.tracker.checkpoint_revert();
                Err(error)
            }
        }
    }

    fn depth(&self) -> usize {
        self.inner.depth()
    }

    fn take_logs(&mut self) -> Vec<Log> {
        self.inner.take_logs()
    }

    fn commit_tx(&mut self) {
        self.inner.commit_tx();
        self.tracker.clear();
    }

    fn discard_tx(&mut self) {
        self.inner.discard_tx();
        self.tracker.clear();
    }

    fn finalize(&mut self) -> Self::State {
        let state = self.inner.finalize();
        self.tracker.clear();
        state
    }
}

impl<J> TrackingJournalExt for TrackingJournal<J> {
    fn tracker(&self) -> &ReserveTracker {
        &self.tracker
    }

    fn tracker_mut(&mut self) -> &mut ReserveTracker {
        &mut self.tracker
    }
}
