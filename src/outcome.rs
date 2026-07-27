use revm_context::result::{EVMError, ExecutionResult, InvalidTransaction};
use std::fmt;

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

impl<DBError: fmt::Display> fmt::Display for GrevmError<DBError> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "transaction {} failed: {}", self.txid, self.error)
    }
}

impl<DBError> std::error::Error for GrevmError<DBError>
where
    DBError: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug)]
    struct TestDbError;

    impl fmt::Display for TestDbError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("test database failure")
        }
    }

    impl std::error::Error for TestDbError {}

    #[test]
    fn grevm_error_displays_transaction_and_evm_error() {
        let error = GrevmError { txid: 42, error: EVMError::Database(TestDbError) };

        assert_eq!(
            error.to_string(),
            "transaction 42 failed: database error: test database failure"
        );
    }

    #[test]
    fn grevm_error_exposes_evm_error_as_source() {
        let error = GrevmError { txid: 7, error: EVMError::Database(TestDbError) };

        let source = std::error::Error::source(&error).expect("EVM error source");
        let evm_error = source
            .downcast_ref::<EVMError<TestDbError>>()
            .expect("source should be the underlying EVM error");
        assert!(matches!(evm_error, EVMError::Database(TestDbError)));

        let database_source = source.source().expect("database error source");
        assert!(database_source.downcast_ref::<TestDbError>().is_some());
    }
}
