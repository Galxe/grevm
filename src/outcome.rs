use revm_context::result::{EVMError, ExecutionResult, InvalidTransaction};

/// Final outcome for one transaction in block order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxExecutionOutcome {
    /// The transaction executed normally, including EVM reverts and halts.
    Executed(ExecutionResult),
    /// The transaction was invalid at the committed state and was applied as a no-op. The exact
    /// revm validation error is forwarded to the consumer for protocol-specific encoding.
    Skipped(InvalidTransaction),
}

/// Error returned when grevm cannot safely continue execution.
#[derive(Debug, Clone)]
pub struct GrevmError<DBError> {
    /// Transaction index associated with the error.
    pub txid: usize,
    /// Underlying EVM error.
    pub error: EVMError<DBError>,
}
