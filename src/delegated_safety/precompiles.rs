use alloy_evm::{
    Database as AlloyDatabase, EvmInternals,
    precompiles::{DynPrecompile, Precompile, PrecompileInput, PrecompilesMap},
};
use revm::{
    context::{Block, Cfg, Context, Journal, LocalContextTr, Transaction},
    handler::PrecompileProvider,
    interpreter::{CallInput, Gas, InputsImpl, InstructionResult, InterpreterResult},
    precompile::{PrecompileError, Precompiles},
};
use revm_primitives::{Address, Bytes};

use super::TrackingJournal;

#[derive(Clone, Debug)]
pub(crate) struct TrackingPrecompilesMap {
    inner: PrecompilesMap,
}

impl TrackingPrecompilesMap {
    pub(crate) fn from_static(precompiles: &'static Precompiles) -> Self {
        Self { inner: PrecompilesMap::from_static(precompiles) }
    }

    pub(crate) fn apply_precompile<F>(&mut self, address: &Address, f: F)
    where
        F: FnOnce(Option<DynPrecompile>) -> Option<DynPrecompile>,
    {
        self.inner.apply_precompile(address, f);
    }
}

impl<BlockEnv, TxEnv, CfgEnv, DB, Chain>
    PrecompileProvider<Context<BlockEnv, TxEnv, CfgEnv, DB, TrackingJournal<Journal<DB>>, Chain>>
    for TrackingPrecompilesMap
where
    BlockEnv: Block,
    TxEnv: Transaction,
    CfgEnv: Cfg,
    DB: AlloyDatabase,
{
    type Output = InterpreterResult;

    fn set_spec(&mut self, _spec: CfgEnv::Spec) -> bool {
        false
    }

    fn run(
        &mut self,
        context: &mut Context<BlockEnv, TxEnv, CfgEnv, DB, TrackingJournal<Journal<DB>>, Chain>,
        address: &Address,
        inputs: &InputsImpl,
        _is_static: bool,
        gas_limit: u64,
    ) -> Result<Option<InterpreterResult>, String> {
        let Some(precompile) = self.inner.get(address) else {
            return Ok(None);
        };

        let mut result = InterpreterResult {
            result: InstructionResult::Return,
            gas: Gas::new(gas_limit),
            output: Bytes::new(),
        };

        let (local, journal) = (&context.local, &mut context.journaled_state);
        let r;
        let input_bytes = match &inputs.input {
            CallInput::SharedBuffer(range) => {
                #[allow(clippy::option_if_let_else)]
                if let Some(slice) = local.shared_memory_buffer_slice(range.clone()) {
                    r = slice;
                    &*r
                } else {
                    &[]
                }
            }
            CallInput::Bytes(bytes) => bytes.as_ref(),
        };

        let precompile_result = precompile.call(PrecompileInput {
            data: input_bytes,
            gas: gas_limit,
            caller: inputs.caller_address,
            value: inputs.call_value,
            internals: EvmInternals::new(journal, &context.block),
            target_address: inputs.target_address,
            bytecode_address: inputs.bytecode_address.expect("always set for precompile calls"),
        });

        match precompile_result {
            Ok(output) => {
                let underflow = result.gas.record_cost(output.gas_used);
                assert!(underflow, "Gas underflow is not possible");
                result.result = if output.reverted {
                    InstructionResult::Revert
                } else {
                    InstructionResult::Return
                };
                result.output = output.bytes;
            }
            Err(PrecompileError::Fatal(e)) => return Err(e),
            Err(e) => {
                result.result = if e.is_oog() {
                    InstructionResult::PrecompileOOG
                } else {
                    InstructionResult::PrecompileError
                };
            }
        };

        Ok(Some(result))
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        Box::new(self.inner.addresses().copied())
    }

    fn contains(&self, address: &Address) -> bool {
        self.inner.get(address).is_some()
    }
}
