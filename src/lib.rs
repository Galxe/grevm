use lazy_static::lazy_static;
use revm::primitives::{Address, EVMError, EVMResultGeneric, ExecutionResult, U256};
use revm::TransitionAccount;
use std::cmp::min;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use tokio::runtime::{Builder, Runtime};
mod hint;
mod partition;
mod scheduler;
mod storage;
mod tx_dependency;

lazy_static! {
    static ref CPU_CORES: usize = thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
}

lazy_static! {
    static ref GREVM_RUNTIME: Runtime = Builder::new_multi_thread()
        // .worker_threads(1) // for debug
        .worker_threads(thread::available_parallelism().map(|n| n.get() * 2).unwrap_or(8))
        .thread_name("grevm-tokio-runtime")
        .enable_all()
        .build()
        .unwrap();
}

static MAX_NUM_ROUND: usize = 3;

type PartitionId = usize;

type TxId = usize;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LocationAndType {
    Basic(Address),
    Storage(Address, U256),
    Code(Address),
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum TransactionStatus {
    Initial,        // tx that has not yet been run once
    Executed,       // tx that are executed
    Unconfirmed,    // tx that is validated but not the continuous ID
    Pending,        // tx that is pending to wait other txs ready
    Conflict,       // tx that is conflicted and need to rerun
    SkipValidation, // tx that can skip validate
    Finality,       // tx that is validated and is the continuous ID
}

pub struct PartitionIndex {
    tx_index: usize,
    partition_id: usize,
}

#[derive(Debug)]
pub enum GrevmError<DBError> {
    EvmError(EVMError<DBError>),
    ExecutionError(String),
    UnreachableError(String),
}

impl<DBError: Display> Display for GrevmError<DBError> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            GrevmError::EvmError(e) => write!(f, "EVM Error: {}", e),
            GrevmError::ExecutionError(e) => write!(f, "Execution Error: {}", e),
            GrevmError::UnreachableError(e) => write!(f, "Unreachable Error: {}", e),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ResultAndTransition {
    /// Status of execution
    pub result: Option<ExecutionResult>,
    /// State that got updated
    pub transition: Vec<(Address, TransitionAccount)>,
    /// Rewards to miner
    pub rewards: u128,
}

pub type GrevmResult<DBError> = EVMResultGeneric<ResultAndTransition, DBError>;

pub use scheduler::*;

pub fn fork_join_util<'scope, F>(num_elements: usize, num_partitions: Option<usize>, f: F)
where
    F: Fn(usize, usize, usize) + Send + Sync + 'scope,
{
    let parallel_cnt = num_partitions.unwrap_or(*CPU_CORES * 2 + 1);
    let index = AtomicUsize::new(0);
    let remaining = num_elements % parallel_cnt;
    let chunk_size = num_elements / parallel_cnt;
    thread::scope(|scope| {
        for _ in 0..parallel_cnt {
            scope.spawn(|| {
                let index = index.fetch_add(1, Ordering::SeqCst);
                let start_pos = chunk_size * index + min(index, remaining);
                let mut end_pos = start_pos + chunk_size;
                if index < remaining {
                    end_pos += 1;
                }
                f(start_pos, end_pos, index);
            });
        }
    });
}
