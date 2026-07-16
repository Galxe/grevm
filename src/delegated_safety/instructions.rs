use revm::{
    bytecode::opcode::{CREATE, CREATE2},
    handler::instructions::EthInstructions,
    interpreter::{
        Host, Instruction, InstructionContext, InstructionResult,
        instructions::contract,
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, InterpreterTypes, RuntimeFlag},
    },
};
use revm_primitives::hardfork::SpecId;

/// Mainnet instructions with grevm-local CREATE/CREATE2 delegated-context guard.
pub(crate) fn gravity_instructions<CTX>() -> EthInstructions<EthInterpreter, CTX>
where
    CTX: Host,
{
    let mut instructions = EthInstructions::new_mainnet();
    instructions.insert_instruction(CREATE, Instruction::new(guarded_create::<_, false, _>, 0));
    instructions.insert_instruction(CREATE2, Instruction::new(guarded_create::<_, true, _>, 0));
    instructions
}

fn guarded_create<WIRE: InterpreterTypes, const IS_CREATE2: bool, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    if context.interpreter.runtime_flag.is_static() {
        context.interpreter.halt(InstructionResult::StateChangeDuringStaticCall);
        return;
    }

    if IS_CREATE2 && !context.interpreter.runtime_flag.spec_id().is_enabled_in(SpecId::PETERSBURG) {
        context.interpreter.halt(InstructionResult::NotActivated);
        return;
    }

    let recipient = context.interpreter.input.target_address();
    let Some(load) = context.host.load_account_delegated(recipient) else {
        context.interpreter.halt(InstructionResult::FatalExternalError);
        return;
    };

    if load.is_delegate_account_cold.is_some() {
        context.interpreter.halt(InstructionResult::NotActivated);
        return;
    }

    contract::create::<WIRE, IS_CREATE2, H>(context);
}
