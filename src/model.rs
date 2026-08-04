use crate::{
    GrevmError,
    beneficiary::{BeneficiaryReadVersion, SpeculativeResult},
};
use ahash::{AHashMap as HashMap, AHashSet as HashSet, RandomState as AHashRandomState};
use dashmap::DashMap;
use revm_context::result::EVMError;
use revm_primitives::{Address, B256, U256};
use revm_state::{AccountInfo, Bytecode};
use std::collections::BTreeMap;

pub(crate) type TxId = usize;
/// Randomized AHash reduces lookup cost on the scheduler's validation-heavy internal hot path.
pub(crate) type MVMemory = DashMap<LocationAndType, BTreeMap<TxId, MemoryEntry>, AHashRandomState>;

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
    Beneficiary(BeneficiaryReadVersion),
    Storage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountBasic {
    pub(crate) balance: U256,
    pub(crate) nonce: u64,
    pub(crate) code_hash: Option<B256>,
}

impl From<&AccountInfo> for AccountBasic {
    fn from(info: &AccountInfo) -> Self {
        Self {
            balance: info.balance,
            nonce: info.nonce,
            code_hash: (!info.is_empty_code_hash()).then_some(info.code_hash),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum MemoryValue {
    /// Absolute account value; `None` means the account does not exist.
    Basic(Option<AccountInfo>),
    Code(Bytecode),
    Storage(U256),
    /// Account deletion or creation masks every older storage slot for this address.
    StorageReset,
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
    StorageReset(Address),
    Code(Address),
}

pub(crate) struct TransactionResult<DBError> {
    pub(crate) read_set: HashMap<LocationAndType, ReadVersion>,
    pub(crate) write_set: HashSet<LocationAndType>,
    pub(crate) execute_result: Result<SpeculativeResult, EVMError<DBError>>,
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
    ParallelError { txid: TxId, message: &'static str },
    FallbackSequential,
}
