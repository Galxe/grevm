use super::{
    DelegatedSafetyConfig, ReservePlan, ReservePlanError, TrackingJournalExt,
    policy::{SharedReservePlan, actual_charged_fee, required_balance},
};
use ahash::AHashMap as HashMap;
use revm::{
    handler::{EvmTr, EvmTrError, FrameResult, FrameTr, Handler, post_execution},
    interpreter::{
        CallOutcome, InstructionResult, InterpreterResult, interpreter_action::FrameInit,
    },
};
use revm_context::{
    BlockEnv, Cfg, ContextTr, JournalTr, Transaction, TransactionType, TxEnv,
    result::{ExecutionResult, HaltReason, InvalidTransaction},
    transaction::AuthorizationTr,
};
use revm_primitives::{Address, Bytes, U256};
use revm_state::EvmState;

use crate::TxId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RewardMode {
    Deferred,
    Immediate,
}

pub(crate) struct GravityHandler<'a, EVM, ERROR, FRAME> {
    txid: TxId,
    config: DelegatedSafetyConfig,
    txs: &'a [TxEnv],
    block: &'a BlockEnv,
    reserve_plan: &'a SharedReservePlan,
    reward_mode: RewardMode,
    _phantom: core::marker::PhantomData<(EVM, ERROR, FRAME)>,
}

impl<'a, EVM, ERROR, FRAME> GravityHandler<'a, EVM, ERROR, FRAME> {
    pub(crate) fn new(
        txid: TxId,
        config: DelegatedSafetyConfig,
        txs: &'a [TxEnv],
        block: &'a BlockEnv,
        reserve_plan: &'a SharedReservePlan,
        reward_mode: RewardMode,
    ) -> Self {
        Self {
            txid,
            config,
            txs,
            block,
            reserve_plan,
            reward_mode,
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<EVM, ERROR, FRAME> Handler for GravityHandler<'_, EVM, ERROR, FRAME>
where
    EVM: EvmTr<
            Context: ContextTr<
                Block = BlockEnv,
                Tx = TxEnv,
                Journal: JournalTr<State = EvmState> + TrackingJournalExt,
            >,
            Frame = FRAME,
        >,
    ERROR: EvmTrError<EVM>,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    type Evm = EVM;
    type Error = ERROR;
    type HaltReason = HaltReason;

    fn run_without_catch_error(
        &mut self,
        evm: &mut Self::Evm,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        let init_and_floor_gas = self.validate(evm)?;
        self.begin_tracking(evm);

        evm.ctx().journal_mut().tracker_mut().start_pre_execution();
        self.validate_against_state_and_deduct_caller(evm)?;
        evm.ctx().journal_mut().tracker_mut().stop_pre_execution();

        self.load_accounts(evm)?;
        let auth_snapshots = self.capture_authority_nonces(evm)?;
        evm.ctx().journal_mut().tracker_mut().start_authorization();
        let eip7702_refund = self.apply_eip7702_auth_list(evm)? as i64;
        self.mark_successful_authorizations(evm, auth_snapshots)?;
        evm.ctx().journal_mut().tracker_mut().stop_authorization();

        let execution_checkpoint = evm.ctx().journal_mut().checkpoint();
        evm.ctx().journal_mut().tracker_mut().start_execution();
        let mut exec_result = self.execution(evm, &init_and_floor_gas)?;
        evm.ctx().journal_mut().tracker_mut().stop_execution();

        self.refund(evm, &mut exec_result, eip7702_refund);
        self.eip7623_check_gas_floor(evm, &mut exec_result, init_and_floor_gas);
        self.reimburse_caller(evm, &mut exec_result)?;

        if self.has_reserve_violation(evm, &exec_result)? {
            let recreate_sender_nonce =
                evm.ctx_ref().journal().tracker().create_tx_sender_nonce_bumped();
            evm.ctx().journal_mut().checkpoint_revert(execution_checkpoint);
            exec_result = reserve_violation_result(&exec_result);
            if recreate_sender_nonce {
                reapply_create_sender_nonce::<EVM, ERROR>(evm)?;
            }
            self.reimburse_caller(evm, &mut exec_result)?;
        } else {
            evm.ctx().journal_mut().checkpoint_commit();
        }

        self.reward_beneficiary(evm, &mut exec_result)?;
        self.execution_result(evm, exec_result)
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        match self.reward_mode {
            RewardMode::Deferred => Ok(()),
            RewardMode::Immediate => {
                post_execution::reward_beneficiary(evm.ctx(), exec_result.gas()).map_err(From::from)
            }
        }
    }
}

impl<EVM, ERROR, FRAME> GravityHandler<'_, EVM, ERROR, FRAME>
where
    EVM: EvmTr<
            Context: ContextTr<
                Block = BlockEnv,
                Tx = TxEnv,
                Journal: JournalTr<State = EvmState> + TrackingJournalExt,
            >,
            Frame = FRAME,
        >,
    ERROR: EvmTrError<EVM>,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    fn begin_tracking(&self, evm: &mut EVM) {
        let caller = evm.ctx_ref().tx().caller();
        let tx_kind_create = evm.ctx_ref().tx().kind().is_create();
        evm.ctx().journal_mut().tracker_mut().begin_transaction(caller, tx_kind_create);
    }

    fn capture_authority_nonces(&self, evm: &mut EVM) -> Result<HashMap<Address, u64>, ERROR> {
        let ctx = evm.ctx_ref();
        let tx = ctx.tx();
        if tx.tx_type() != TransactionType::Eip7702 as u8 {
            return Ok(HashMap::new());
        }

        let chain_id = ctx.cfg().chain_id();
        let authorities = tx
            .authorization_list()
            .filter(|authorization| {
                let auth_chain_id = authorization.chain_id();
                (auth_chain_id.is_zero() || auth_chain_id == U256::from(chain_id)) &&
                    authorization.nonce() != u64::MAX
            })
            .filter_map(|authorization| authorization.authority())
            .collect::<Vec<_>>();

        let mut snapshots = HashMap::with_capacity(authorities.len());
        for authority in authorities {
            if snapshots.contains_key(&authority) {
                continue;
            }
            let nonce = evm.ctx().journal_mut().load_account_code(authority)?.data.info.nonce;
            snapshots.insert(authority, nonce);
        }
        Ok(snapshots)
    }

    fn mark_successful_authorizations(
        &self,
        evm: &mut EVM,
        snapshots: HashMap<Address, u64>,
    ) -> Result<(), ERROR> {
        for (authority, before_nonce) in snapshots {
            let after_nonce = evm.ctx().journal_mut().load_account_code(authority)?.data.info.nonce;
            if after_nonce > before_nonce {
                evm.ctx().journal_mut().tracker_mut().mark_authorization_sensitive(authority);
            }
        }
        Ok(())
    }

    fn reserve_plan(&self) -> Result<&ReservePlan, ERROR> {
        self.reserve_plan
            .get_or_init(|| ReservePlan::build(self.txs, self.block))
            .as_ref()
            .map_err(plan_error_to_evm_error::<EVM, ERROR>)
    }

    fn has_reserve_violation(
        &self,
        evm: &mut EVM,
        exec_result: &FrameResult,
    ) -> Result<bool, ERROR> {
        let candidates = evm.ctx_ref().journal().tracker().protected_debits();
        if candidates.is_empty() {
            return Ok(false);
        }

        let plan = self.reserve_plan()?;
        let charged_fee =
            actual_charged_fee(evm.ctx_ref().tx(), evm.ctx_ref().block(), exec_result.gas().used());
        let sender = evm.ctx_ref().tx().caller();

        for address in candidates {
            let original_balance = evm.ctx_ref().journal().tracker().original_balance(address);
            let final_balance = evm.ctx().journal_mut().load_account(address)?.data.info.balance;
            let original_balance = original_balance.unwrap_or(final_balance);
            let required = required_balance(
                &self.config,
                plan,
                self.txid,
                address,
                original_balance,
                address == sender,
                charged_fee,
            );
            if final_balance < required {
                return Ok(true);
            }
        }

        Ok(false)
    }
}

fn reserve_violation_result(result: &FrameResult) -> FrameResult {
    let mut gas = *result.gas();
    gas.set_refund(0);
    FrameResult::Call(CallOutcome::new(
        InterpreterResult { result: InstructionResult::Revert, output: Bytes::new(), gas },
        0..0,
    ))
}

fn reapply_create_sender_nonce<EVM, ERROR>(evm: &mut EVM) -> Result<(), ERROR>
where
    EVM: EvmTr<
        Context: ContextTr<Tx = TxEnv, Journal: JournalTr<State = EvmState> + TrackingJournalExt>,
    >,
    ERROR: EvmTrError<EVM>,
{
    let sender = evm.ctx_ref().tx().caller();
    let account = evm.ctx().journal_mut().load_account(sender)?;
    let Some(new_nonce) = account.data.info.nonce.checked_add(1) else {
        return Err(InvalidTransaction::NonceOverflowInTransaction.into());
    };
    account.data.info.nonce = new_nonce;
    evm.ctx().journal_mut().nonce_bump_journal_entry(sender);
    Ok(())
}

fn plan_error_to_evm_error<EVM, ERROR>(_error: &ReservePlanError) -> ERROR
where
    EVM: EvmTr,
    ERROR: EvmTrError<EVM>,
{
    InvalidTransaction::OverflowPaymentInTransaction.into()
}
