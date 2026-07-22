use crate::GrevmError;
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use dashmap::DashMap;
use revm_context::result::{EVMError, ResultAndState};
use revm_primitives::{Address, B256, U256};
use revm_state::{AccountInfo, Bytecode};
use std::collections::BTreeMap;

pub(crate) type TxId = usize;
pub(crate) type MVMemory = DashMap<LocationAndType, BTreeMap<TxId, MemoryEntry>>;

#[derive(Debug, Clone, Eq, PartialEq, Default)]
pub(crate) enum TransactionStatus {
    #[default]
    Initial,
    Executing,
    Executed,
    Validating,
    Unconfirmed,
    Conflict,
    Finality,
}

#[derive(Debug, Default)]
pub(crate) struct TxState {
    pub(crate) status: TransactionStatus,
    pub(crate) incarnation: usize,
    pub(crate) dependency: Option<TxId>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TxVersion {
    pub(crate) txid: TxId,
    pub(crate) incarnation: usize,
}

impl TxVersion {
    pub(crate) fn new(txid: TxId, incarnation: usize) -> Self {
        Self { txid, incarnation }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum ReadVersion {
    MvMemory(TxVersion),
    Storage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountBasic {
    pub(crate) balance: U256,
    pub(crate) nonce: u64,
    pub(crate) code_hash: Option<B256>,
}

#[derive(Debug, Clone)]
pub(crate) enum MemoryValue {
    Basic(AccountInfo),
    /// Code plus whether the transaction created the account and therefore cleared its storage.
    Code(Bytecode, bool),
    Storage(U256),
    SelfDestructed,
}

#[derive(Debug, Clone)]
pub(crate) struct MemoryEntry {
    pub(crate) incarnation: usize,
    pub(crate) data: MemoryValue,
    pub(crate) estimate: bool,
}

impl MemoryEntry {
    pub(crate) fn new(incarnation: usize, data: MemoryValue, estimate: bool) -> Self {
        Self { incarnation, data, estimate }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LocationAndType {
    Basic(Address),
    Storage(Address, U256),
    Code(Address),
}

pub(crate) struct TransactionResult<DBError> {
    pub(crate) read_set: HashMap<LocationAndType, ReadVersion>,
    pub(crate) write_set: HashSet<LocationAndType>,
    pub(crate) execute_result: Result<ResultAndState, EVMError<DBError>>,
}

#[derive(Clone, Debug)]
pub(crate) enum Task {
    Execution(TxVersion),
    Validation(TxVersion),
}

impl Default for Task {
    fn default() -> Self {
        Self::Execution(TxVersion::new(0, 0))
    }
}

pub(crate) enum AbortReason<DBError> {
    FatalEvmError(TxId),
    CommitError(GrevmError<DBError>),
    ParallelError {
        txid: TxId,
        message: &'static str,
    },
    #[allow(dead_code)]
    SelfDestructed,
    FallbackSequential,
}
