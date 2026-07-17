use revm_context::result::{EVMError, ExecutionResult, InvalidTransaction};

/// Stable reason recorded when a transaction is skipped without changing state.
///
/// The numeric values are part of the consumer-facing protocol and must not be reordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum SkipReason {
    /// The transaction nonce is lower than the committed sender nonce.
    NonceTooLow = 1,
    /// The transaction nonce is higher than the committed sender nonce.
    NonceTooHigh = 2,
    /// The sender cannot cover the transaction's maximum upfront cost.
    InsufficientFunds = 3,
    /// The sender is rejected by EIP-3607 because it has non-delegated code.
    SenderNotEoa = 4,
}

impl SkipReason {
    /// Classifies transaction errors that are safe to treat as a state-free skip.
    pub fn from_evm_error<DBError>(error: &EVMError<DBError>) -> Option<Self> {
        let EVMError::Transaction(error) = error else { return None };
        match error {
            InvalidTransaction::NonceTooLow { .. } => Some(Self::NonceTooLow),
            InvalidTransaction::NonceTooHigh { .. } => Some(Self::NonceTooHigh),
            InvalidTransaction::LackOfFundForMaxFee { .. } => Some(Self::InsufficientFunds),
            InvalidTransaction::RejectCallerWithCode => Some(Self::SenderNotEoa),
            _ => None,
        }
    }
}

/// Final outcome for one transaction in block order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxExecutionOutcome {
    /// The transaction executed normally, including EVM reverts and halts.
    Executed(ExecutionResult),
    /// The transaction was invalid at the committed state and was applied as a no-op.
    Skipped(SkipReason),
}

/// Error returned when grevm cannot safely continue execution.
#[derive(Debug, Clone)]
pub struct GrevmError<DBError> {
    /// Transaction index associated with the error.
    pub txid: usize,
    /// Underlying EVM error.
    pub error: EVMError<DBError>,
}
