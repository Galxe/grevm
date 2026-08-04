use crate::{
    AccountBasic, LocationAndType, MVMemory, MemoryEntry, MemoryValue, ReadVersion, TxId,
    TxVersion, account::FinalizedAccount, beneficiary::Beneficiary,
};
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use revm::{Database, DatabaseRef};
use revm_primitives::{Address, B256, U256};
use revm_state::{AccountInfo, Bytecode, EvmState};

/// Reusable revm database adapter for the currently executing transaction incarnation.
///
/// Reads resolve against preceding MV-memory versions, the block beneficiary history, and then
/// the backing database. The adapter also records the incarnation's validation inputs and
/// publishes its writes when execution finishes.
#[derive(Debug)]
pub(crate) struct IncarnationDb<'a, DB>
where
    DB: DatabaseRef,
{
    beneficiary: &'a Beneficiary,
    backing_db: &'a DB,
    mv_memory: &'a MVMemory,

    read_set: HashMap<LocationAndType, ReadVersion>,
    account_snapshots: HashMap<Address, AccountBasic>,
    version: TxVersion,
    blocking_txs: HashSet<TxId>,
    blocked_by_beneficiary: bool,
}

/// Validation accesses and blocking metadata collected for one incarnation.
pub(crate) struct IncarnationAccesses {
    pub(crate) read_set: HashMap<LocationAndType, ReadVersion>,
    pub(crate) write_set: HashSet<LocationAndType>,
    pub(crate) blocking_txs: HashSet<TxId>,
    pub(crate) blocked_by_beneficiary: bool,
}

impl IncarnationAccesses {
    pub(crate) fn is_blocked(&self) -> bool {
        !self.blocking_txs.is_empty()
    }
}

impl<'a, DB> IncarnationDb<'a, DB>
where
    DB: DatabaseRef,
{
    pub(crate) fn new(
        backing_db: &'a DB,
        mv_memory: &'a MVMemory,
        beneficiary: &'a Beneficiary,
    ) -> Self {
        Self {
            beneficiary,
            backing_db,
            mv_memory,
            read_set: HashMap::new(),
            account_snapshots: HashMap::new(),
            version: TxVersion::new(0, 0),
            blocking_txs: HashSet::new(),
            blocked_by_beneficiary: false,
        }
    }

    /// Whether this incarnation database is coordinated by the beneficiary for `address`.
    pub(crate) fn beneficiary_matches(&self, address: Address) -> bool {
        self.beneficiary.matches(address)
    }

    /// Reset scratch state and start collecting accesses for `version`.
    pub(crate) fn begin_incarnation(&mut self, version: TxVersion) {
        debug_assert!(self.read_set.is_empty(), "previous incarnation was not finished");
        debug_assert!(self.account_snapshots.is_empty(), "previous incarnation was not finished");
        debug_assert!(self.blocking_txs.is_empty(), "previous incarnation was not finished");
        debug_assert!(!self.blocked_by_beneficiary, "previous incarnation was not finished");
        self.version = version;
        self.read_set.clear();
        self.account_snapshots.clear();
        self.blocking_txs.clear();
        self.blocked_by_beneficiary = false;
    }

    /// Finish a successful incarnation and publish its writes to multi-version memory.
    ///
    /// A read from an estimated predecessor makes this incarnation an estimate as well. Deriving
    /// that status here keeps dependency reporting and write publication consistent.
    pub(crate) fn finish_incarnation(&mut self, changes: &EvmState) -> IncarnationAccesses {
        let estimate = !self.blocking_txs.is_empty();
        let write_set = self.publish_writes(changes, estimate);
        self.account_snapshots.clear();
        IncarnationAccesses {
            read_set: std::mem::take(&mut self.read_set),
            write_set,
            blocking_txs: std::mem::take(&mut self.blocking_txs),
            blocked_by_beneficiary: std::mem::take(&mut self.blocked_by_beneficiary),
        }
    }

    /// Discard EVM writes from a failed incarnation while preserving its discovered blockers.
    pub(crate) fn discard_incarnation(&mut self) -> IncarnationAccesses {
        // Failed executions are never validated, so retain the read-set allocation for the next
        // incarnation instead of moving and immediately dropping it in the scheduler.
        self.read_set.clear();
        self.account_snapshots.clear();
        IncarnationAccesses {
            read_set: HashMap::new(),
            write_set: HashSet::new(),
            blocking_txs: std::mem::take(&mut self.blocking_txs),
            blocked_by_beneficiary: std::mem::take(&mut self.blocked_by_beneficiary),
        }
    }

    fn publish_writes(&self, changes: &EvmState, estimate: bool) -> HashSet<LocationAndType> {
        let mut write_set = HashSet::new();
        for (address, account) in changes {
            let (info, created) = match FinalizedAccount::from(account) {
                FinalizedAccount::Unchanged => continue,
                FinalizedAccount::Deleted => {
                    if !self.beneficiary.matches(*address) {
                        self.publish_value(
                            LocationAndType::Basic(*address),
                            MemoryValue::Basic(None),
                            estimate,
                            &mut write_set,
                        );
                    }
                    self.publish_storage_reset(*address, estimate, &mut write_set);
                    continue
                }
                FinalizedAccount::Created(info) => (info, true),
                FinalizedAccount::Updated(info) => (info, false),
            };

            if created {
                self.publish_storage_reset(*address, estimate, &mut write_set);
            }

            let account_snapshot = self.account_snapshots.get(address);
            let has_code = !info.is_empty_code_hash();
            // The account's code was set or changed in this transaction. Besides the usual
            // EOA/CREATE "code appears for the first time" case, EIP-7702 lets an account's
            // delegation be re-pointed in a later transaction of the same block, so the
            // post-state `code_hash` can move from one non-empty value to another. We must
            // (re)publish the `Code` entry whenever the post-state code_hash differs from what
            // we read; otherwise later txs in the block resolve a stale delegation target.
            let code_changed = has_code &&
                info.code.is_some() &&
                account_snapshot.is_none_or(|basic| basic.code_hash != Some(info.code_hash));
            if code_changed {
                // Storage lifecycle is independent: CREATE resets old slots, while EIP-7702
                // (re)delegation changes code without clearing storage.
                self.publish_value(
                    LocationAndType::Code(*address),
                    MemoryValue::Code(info.code.clone().unwrap()),
                    estimate,
                    &mut write_set,
                );
            }

            if !self.beneficiary.matches(*address) &&
                (code_changed ||
                    account_snapshot.is_none_or(|basic| {
                        basic.nonce != info.nonce || basic.balance != info.balance
                    }))
            {
                self.publish_value(
                    LocationAndType::Basic(*address),
                    MemoryValue::Basic(Some(AccountInfo { code: None, ..info.clone() })),
                    estimate,
                    &mut write_set,
                );
            }

            for (slot, value) in account.changed_storage_slots() {
                self.publish_value(
                    LocationAndType::Storage(*address, *slot),
                    MemoryValue::Storage(value.present_value),
                    estimate,
                    &mut write_set,
                );
            }
        }

        write_set
    }

    fn publish_storage_reset(
        &self,
        address: Address,
        estimate: bool,
        write_set: &mut HashSet<LocationAndType>,
    ) {
        self.publish_value(
            LocationAndType::StorageReset(address),
            MemoryValue::StorageReset,
            estimate,
            write_set,
        );
    }

    fn publish_value(
        &self,
        location: LocationAndType,
        value: MemoryValue,
        estimate: bool,
        write_set: &mut HashSet<LocationAndType>,
    ) {
        write_set.insert(location.clone());
        self.mv_memory
            .entry(location)
            .or_default()
            .insert(self.version.txid, MemoryEntry::new(self.version.incarnation, value, estimate));
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
        if let Some(written_transactions) = self.mv_memory.get(&location) &&
            let Some((&txid, entry)) =
                written_transactions.range(..self.version.txid).next_back() &&
            let MemoryValue::Code(code) = &entry.data
        {
            result = Some(code.clone());
            if entry.estimate {
                self.blocking_txs.insert(txid);
            }
            read_version = ReadVersion::MvMemory(TxVersion::new(txid, entry.incarnation));
        }
        // 2. read from database
        if result.is_none() {
            let byte_code = self.backing_db.code_by_hash_ref(code_hash)?;
            result = Some(byte_code);
        }

        self.read_set.insert(location, read_version);
        Ok(result.expect("No bytecode"))
    }
}

impl<'a, DB> Database for IncarnationDb<'a, DB>
where
    DB: DatabaseRef,
{
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let mut result = None;
        if self.beneficiary.matches(address) {
            let location = LocationAndType::Basic(address);
            match self.beneficiary.resolve_before(self.version.txid) {
                Ok(read) => {
                    let (account, version) = read.into_parts();
                    result = account;
                    self.read_set.insert(location, ReadVersion::Beneficiary(version));
                    if let Some(info) = &result {
                        self.account_snapshots.insert(address, AccountBasic::from(info));
                    }
                }
                Err(blocker) => {
                    self.blocking_txs.insert(blocker);
                    self.blocked_by_beneficiary = true;
                    // This incarnation will be discarded. Absence lets the EVM finish without
                    // reading the mutable committed cache as a second, non-atomic history anchor.
                }
            }
        } else {
            let mut read_version = ReadVersion::Storage;
            let mut read_account = None;
            let location = LocationAndType::Basic(address);
            // 1. read from multi-version memory
            if let Some(written_transactions) = self.mv_memory.get(&location) &&
                let Some((&txid, entry)) =
                    written_transactions.range(..self.version.txid).next_back() &&
                let MemoryValue::Basic(account) = &entry.data
            {
                result = account.clone();
                read_account = result.as_ref().map(AccountBasic::from);
                if entry.estimate {
                    self.blocking_txs.insert(txid);
                }
                read_version = ReadVersion::MvMemory(TxVersion::new(txid, entry.incarnation));
            }
            // 2. read from database
            if matches!(read_version, ReadVersion::Storage) {
                result = self.backing_db.basic_ref(address)?;
                read_account = result.as_ref().map(AccountBasic::from);
            }
            if let Some(read_account) = read_account {
                self.account_snapshots.insert(address, read_account);
            }
            self.read_set.insert(location, read_version);
        }

        if let Some(info) = &mut result &&
            !info.is_empty_code_hash() &&
            info.code.is_none()
        {
            info.code = Some(self.code_by_address(address, info.code_hash)?);
        }
        Ok(result)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.backing_db.code_by_hash_ref(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let reset_location = LocationAndType::StorageReset(address);
        let mut reset_version = ReadVersion::Storage;
        let mut reset_txid = None;
        if let Some(writes) = self.mv_memory.get(&reset_location) &&
            let Some((&txid, entry)) = writes.range(..self.version.txid).next_back() &&
            matches!(entry.data, MemoryValue::StorageReset)
        {
            reset_txid = Some(txid);
            if entry.estimate {
                self.blocking_txs.insert(txid);
            }
            reset_version = ReadVersion::MvMemory(TxVersion::new(txid, entry.incarnation));
        }
        self.read_set.insert(reset_location, reset_version);

        let location = LocationAndType::Storage(address, index);
        let mut slot_version = ReadVersion::Storage;
        let mut slot_write = None;
        if let Some(writes) = self.mv_memory.get(&location) &&
            let Some((&txid, entry)) = writes.range(..self.version.txid).next_back() &&
            let MemoryValue::Storage(value) = entry.data
        {
            if entry.estimate {
                self.blocking_txs.insert(txid);
            }
            slot_version = ReadVersion::MvMemory(TxVersion::new(txid, entry.incarnation));
            slot_write = Some((txid, value));
        }
        self.read_set.insert(location, slot_version);

        if let Some((slot_txid, value)) = slot_write &&
            reset_txid.is_none_or(|reset_txid| slot_txid >= reset_txid)
        {
            return Ok(value);
        }
        if reset_txid.is_some() {
            return Ok(U256::ZERO);
        }
        self.backing_db.storage_ref(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.backing_db.block_hash_ref(number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParallelState;
    use revm_database::EmptyDB;
    use revm_state::Account;

    fn address(last: u8) -> Address {
        Address::with_last_byte(last)
    }

    fn publish(memory: &MVMemory, location: LocationAndType, txid: TxId, value: MemoryValue) {
        memory.entry(location).or_default().insert(txid, MemoryEntry::new(1, value, false));
    }

    #[test]
    fn beneficiary_storage_is_independent_from_unresolved_balance_rewards() {
        let beneficiary = address(1);
        let memory = MVMemory::default();
        publish(
            &memory,
            LocationAndType::Storage(beneficiary, U256::ZERO),
            0,
            MemoryValue::Storage(U256::from(7)),
        );
        // Beneficiary history for tx 0 deliberately remains unresolved. A storage-only read must
        // not inspect it.
        let beneficiary_state = Beneficiary::new(beneficiary, None, 2);
        let backing_db = EmptyDB::default();
        let mut db = IncarnationDb::new(&backing_db, &memory, &beneficiary_state);
        db.begin_incarnation(TxVersion::new(1, 1));

        assert_eq!(db.storage(beneficiary, U256::ZERO).unwrap(), U256::from(7));
        let accesses = db.finish_incarnation(&EvmState::default());
        assert!(!accesses.is_blocked());
        assert!(!accesses.blocked_by_beneficiary);
    }

    #[test]
    fn finish_incarnation_derives_estimate_from_blockers() {
        let beneficiary = address(1);
        let account = address(2);
        let memory = MVMemory::default();
        memory
            .entry(LocationAndType::Basic(account))
            .or_default()
            .insert(0, MemoryEntry::new(1, MemoryValue::Basic(Some(AccountInfo::default())), true));
        let beneficiary_state = Beneficiary::new(beneficiary, None, 2);
        let backing_db = EmptyDB::default();
        let mut db = IncarnationDb::new(&backing_db, &memory, &beneficiary_state);
        db.begin_incarnation(TxVersion::new(1, 1));

        let mut info = db.basic(account).unwrap().unwrap();
        info.balance = U256::from(1);
        let mut changed = Account::from(info);
        changed.mark_touch();
        let mut changes = EvmState::default();
        changes.insert(account, changed);

        let accesses = db.finish_incarnation(&changes);
        assert!(accesses.is_blocked());
        assert!(accesses.blocking_txs.contains(&0));
        assert!(accesses.write_set.contains(&LocationAndType::Basic(account)));
        assert!(memory.get(&LocationAndType::Basic(account)).unwrap().get(&1).unwrap().estimate);
    }

    #[test]
    fn discard_incarnation_returns_dependencies_without_publishing_writes() {
        let beneficiary = address(1);
        let account = address(2);
        let memory = MVMemory::default();
        memory
            .entry(LocationAndType::Basic(account))
            .or_default()
            .insert(0, MemoryEntry::new(1, MemoryValue::Basic(Some(AccountInfo::default())), true));
        let beneficiary_state = Beneficiary::new(beneficiary, None, 2);
        let backing_db = EmptyDB::default();
        let mut db = IncarnationDb::new(&backing_db, &memory, &beneficiary_state);
        db.begin_incarnation(TxVersion::new(1, 1));
        db.basic(account).unwrap();

        let accesses = db.discard_incarnation();
        assert!(accesses.is_blocked());
        assert!(accesses.read_set.is_empty());
        assert!(accesses.write_set.is_empty());
        assert!(!memory.get(&LocationAndType::Basic(account)).unwrap().contains_key(&1));

        // Finishing leaves the reusable adapter ready for the next incarnation.
        db.begin_incarnation(TxVersion::new(1, 2));
        db.finish_incarnation(&EvmState::default());
    }

    #[test]
    fn raw_beneficiary_storage_uses_the_preloaded_base_view() {
        let beneficiary = address(1);
        let mut state = ParallelState::new(EmptyDB::default(), true, false);
        state.insert_account_with_storage(
            beneficiary,
            AccountInfo::default(),
            [(U256::ZERO, U256::from(9))].into_iter().collect(),
        );
        let (state_view, _commit_state) = state.split_for_parallel();
        let anchor = state_view.basic_ref(beneficiary).unwrap();
        let beneficiary_state = Beneficiary::new(beneficiary, anchor, 1);
        let memory = MVMemory::default();
        let mut db = IncarnationDb::new(&state_view, &memory, &beneficiary_state);
        db.begin_incarnation(TxVersion::new(0, 1));

        assert_eq!(db.storage(beneficiary, U256::ZERO).unwrap(), U256::from(9));
        db.finish_incarnation(&EvmState::default());
    }

    #[test]
    fn storage_reset_masks_older_slots_without_waiting_for_commit() {
        let beneficiary = address(1);
        let account = address(2);
        let memory = MVMemory::default();
        publish(
            &memory,
            LocationAndType::Storage(account, U256::ZERO),
            0,
            MemoryValue::Storage(U256::from(7)),
        );
        publish(&memory, LocationAndType::StorageReset(account), 1, MemoryValue::StorageReset);
        let beneficiary_state = Beneficiary::new(beneficiary, None, 3);
        let backing_db = EmptyDB::default();
        let mut db = IncarnationDb::new(&backing_db, &memory, &beneficiary_state);
        db.begin_incarnation(TxVersion::new(2, 1));

        assert_eq!(db.storage(account, U256::ZERO).unwrap(), U256::ZERO);
        let accesses = db.finish_incarnation(&EvmState::default());
        assert_eq!(
            accesses.read_set.get(&LocationAndType::StorageReset(account)),
            Some(&ReadVersion::MvMemory(TxVersion::new(1, 1))),
        );
    }

    #[test]
    fn same_transaction_created_storage_wins_over_its_reset_marker() {
        let beneficiary = address(1);
        let account = address(2);
        let memory = MVMemory::default();
        publish(&memory, LocationAndType::StorageReset(account), 0, MemoryValue::StorageReset);
        publish(
            &memory,
            LocationAndType::Storage(account, U256::ZERO),
            0,
            MemoryValue::Storage(U256::from(9)),
        );
        let beneficiary_state = Beneficiary::new(beneficiary, None, 2);
        let backing_db = EmptyDB::default();
        let mut db = IncarnationDb::new(&backing_db, &memory, &beneficiary_state);
        db.begin_incarnation(TxVersion::new(1, 1));

        assert_eq!(db.storage(account, U256::ZERO).unwrap(), U256::from(9));
        db.finish_incarnation(&EvmState::default());
    }

    #[test]
    fn beneficiary_basic_folds_reward_history_from_immutable_anchor() {
        let beneficiary = address(1);
        let memory = MVMemory::default();
        let beneficiary_state = Beneficiary::new(
            beneficiary,
            Some(AccountInfo { balance: U256::from(10), ..Default::default() }),
            2,
        );
        assert!(beneficiary_state.record_reward_for_test(&TxVersion::new(0, 1), U256::from(3)));
        let backing_db = EmptyDB::default();
        let mut db = IncarnationDb::new(&backing_db, &memory, &beneficiary_state);
        db.begin_incarnation(TxVersion::new(1, 1));

        assert_eq!(db.basic(beneficiary).unwrap().unwrap().balance, U256::from(13));
        let accesses = db.finish_incarnation(&EvmState::default());
        assert!(matches!(
            accesses.read_set.get(&LocationAndType::Basic(beneficiary)),
            Some(ReadVersion::Beneficiary(_)),
        ));
    }
}
