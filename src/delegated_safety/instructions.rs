use revm::{
    bytecode::opcode::{CREATE, CREATE2},
    handler::instructions::EthInstructions,
    interpreter::{
        Host, Instruction, InstructionContext, InstructionExecResult, InstructionResult,
        instructions::contract,
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, InterpreterTypes, RuntimeFlag},
    },
};
use revm_primitives::hardfork::SpecId;

/// Mainnet instructions with grevm-local CREATE/CREATE2 delegated-context guard.
pub(crate) fn gravity_instructions<CTX>(spec: SpecId) -> EthInstructions<EthInterpreter, CTX>
where
    CTX: Host,
{
    let mut instructions = EthInstructions::new_mainnet_with_spec(spec);
    instructions.insert_instruction(CREATE, Instruction::new(guarded_create::<false, _, _>), 0);
    instructions.insert_instruction(CREATE2, Instruction::new(guarded_create::<true, _, _>), 0);
    instructions
}

fn guarded_create<const IS_CREATE2: bool, WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) -> InstructionExecResult {
    if context.interpreter.runtime_flag.is_static() {
        return Err(InstructionResult::StateChangeDuringStaticCall)
    }

    if IS_CREATE2 && !context.interpreter.runtime_flag.spec_id().is_enabled_in(SpecId::PETERSBURG) {
        return Err(InstructionResult::NotActivated)
    }

    // `target_address` is the account owning this execution context. For a 7702 call it remains
    // the delegated EOA even though the interpreter executes bytecode loaded from its delegate.
    let recipient = context.interpreter.input.target_address();
    let Some(load) = context.host.load_account_delegated(recipient) else {
        return Err(InstructionResult::FatalExternalError)
    };

    // `Some(coldness)` means the target has an EIP-7702 delegation designator; the boolean itself
    // only reports whether loading the delegate was cold and is irrelevant to this policy.
    if load.is_delegate_account_cold.is_some() {
        return Err(InstructionResult::NotActivated)
    }

    contract::create::<IS_CREATE2, WIRE, H>(context)
}
