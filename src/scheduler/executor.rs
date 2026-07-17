//! EVM construction and transaction driving for the scheduler.
//!
//! The scheduler owns ordering, conflicts and MV-memory. This module owns revm-specific types and
//! exposes one small interface so standard and delegated-safe execution share the same scheduling
//! state machine.

use crate::{
    ParallelState, TxVersion,
    cache_db::CacheDB,
    delegated_safety::{
        DelegatedSafetyConfig, GravityHandler, RewardMode, SharedReservePlan, TrackingJournal,
        TrackingPrecompilesMap, gravity_instructions,
    },
};
use alloy_evm::{
    Database as AlloyDatabase,
    precompiles::{DynPrecompile, PrecompilesMap},
};
use revm::{
    Context, DatabaseRef, ExecuteEvm, MainBuilder, MainContext,
    context::Evm as RevmEvm,
    handler::{
        EthFrame, EvmTr, EvmTrError, FrameResult, FrameTr, Handler, instructions::EthInstructions,
    },
    interpreter::{interpreter::EthInterpreter, interpreter_action::FrameInit},
    precompile::{PrecompileSpecId, Precompiles},
};
use revm_context::{
    BlockEnv, CfgEnv, ContextSetters, ContextTr, Journal, JournalTr, TxEnv,
    result::{EVMError, HaltReason, ResultAndState},
};
use revm_inspector::NoOpInspector;
use revm_primitives::Address;
use revm_state::EvmState;
use std::sync::Arc;

type StandardContext<'a, DB> = Context<BlockEnv, TxEnv, CfgEnv, CacheDB<'a, ParallelState<DB>>>;

type StandardEvm<'a, DB> = RevmEvm<
    StandardContext<'a, DB>,
    NoOpInspector,
    EthInstructions<EthInterpreter, StandardContext<'a, DB>>,
    PrecompilesMap,
    EthFrame,
>;

type SafetyContext<DB> = Context<BlockEnv, TxEnv, CfgEnv, DB, TrackingJournal<Journal<DB>>>;

type SafetyEvm<DB> = RevmEvm<
    SafetyContext<DB>,
    NoOpInspector,
    EthInstructions<EthInterpreter, SafetyContext<DB>>,
    TrackingPrecompilesMap,
    EthFrame,
>;

/// The only EVM operations needed by the parallel scheduling state machine.
pub(crate) trait ParallelTransactionExecutor<'db, DB>
where
    DB: DatabaseRef,
{
    fn transact(
        &mut self,
        version: TxVersion,
        tx: TxEnv,
    ) -> Result<ResultAndState, EVMError<DB::Error>>;

    fn db_mut(&mut self) -> &mut CacheDB<'db, ParallelState<DB>>;
}

pub(crate) struct StandardExecutor<'a, DB>
where
    DB: DatabaseRef,
{
    evm: StandardEvm<'a, DB>,
}

impl<'a, DB> StandardExecutor<'a, DB>
where
    DB: DatabaseRef,
    DB::Error: Send + Sync + 'static,
{
    pub(crate) fn new(
        db: CacheDB<'a, ParallelState<DB>>,
        cfg: CfgEnv,
        block: BlockEnv,
        custom_precompiles: &[(Address, DynPrecompile)],
    ) -> Self {
        let spec = cfg.spec;
        let mut evm = Context::mainnet()
            .with_db(db)
            .with_cfg(cfg)
            .with_block(block)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
                PrecompileSpecId::from_spec_id(spec),
            )));
        apply_standard_precompiles(&mut evm, custom_precompiles);
        Self { evm }
    }
}

impl<'a, DB> ParallelTransactionExecutor<'a, DB> for StandardExecutor<'a, DB>
where
    DB: DatabaseRef,
    DB::Error: Send + Sync + 'static,
{
    fn transact(
        &mut self,
        version: TxVersion,
        tx: TxEnv,
    ) -> Result<ResultAndState, EVMError<DB::Error>> {
        self.evm.db_mut().reset_state(version);
        self.evm.ctx.set_tx(tx);
        let output = NoRewardHandler::default().run(&mut self.evm);
        let state = self.evm.finalize();
        output.map(|result| ResultAndState { result, state })
    }

    fn db_mut(&mut self) -> &mut CacheDB<'a, ParallelState<DB>> {
        self.evm.db_mut()
    }
}

pub(crate) struct SafetyExecutor<'a, DB>
where
    DB: DatabaseRef,
{
    evm: SafetyEvm<CacheDB<'a, ParallelState<DB>>>,
    config: DelegatedSafetyConfig,
    txs: Arc<Vec<TxEnv>>,
    block: BlockEnv,
    reserve_plan: SharedReservePlan,
}

impl<'a, DB> SafetyExecutor<'a, DB>
where
    DB: DatabaseRef,
    DB::Error: Send + Sync + 'static,
{
    pub(crate) fn new(
        db: CacheDB<'a, ParallelState<DB>>,
        cfg: CfgEnv,
        block: BlockEnv,
        custom_precompiles: &[(Address, DynPrecompile)],
        config: DelegatedSafetyConfig,
        txs: Arc<Vec<TxEnv>>,
        reserve_plan: SharedReservePlan,
    ) -> Self {
        let evm = build_safety_evm(db, cfg, block.clone(), custom_precompiles);
        Self { evm, config, txs, block, reserve_plan }
    }
}

impl<'a, DB> ParallelTransactionExecutor<'a, DB> for SafetyExecutor<'a, DB>
where
    DB: DatabaseRef,
    DB::Error: Send + Sync + 'static,
{
    fn transact(
        &mut self,
        version: TxVersion,
        tx: TxEnv,
    ) -> Result<ResultAndState, EVMError<DB::Error>> {
        self.evm.db_mut().reset_state(version.clone());
        self.evm.ctx.set_tx(tx);
        let output = GravityHandler::new(
            version.txid,
            self.config.clone(),
            self.txs.as_slice(),
            &self.block,
            &self.reserve_plan,
            RewardMode::Deferred,
        )
        .run(&mut self.evm);
        let state = self.evm.finalize();
        output.map(|result| ResultAndState { result, state })
    }

    fn db_mut(&mut self) -> &mut CacheDB<'a, ParallelState<DB>> {
        self.evm.db_mut()
    }
}

pub(crate) fn build_safety_evm<DB>(
    db: DB,
    cfg: CfgEnv,
    block: BlockEnv,
    custom_precompiles: &[(Address, DynPrecompile)],
) -> SafetyEvm<DB>
where
    DB: AlloyDatabase,
{
    let spec = cfg.spec;
    let mut evm =
        Context::<BlockEnv, TxEnv, CfgEnv, DB, TrackingJournal<Journal<DB>>>::new(db, spec)
            .with_cfg(cfg)
            .with_block(block)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(TrackingPrecompilesMap::from_static(Precompiles::new(
                PrecompileSpecId::from_spec_id(spec),
            )));
    evm.instruction = gravity_instructions();

    for (address, precompile) in custom_precompiles {
        let precompile = precompile.clone();
        evm.precompiles.apply_precompile(address, move |_| Some(precompile));
    }
    evm
}

fn apply_standard_precompiles<DB>(
    evm: &mut StandardEvm<'_, DB>,
    custom_precompiles: &[(Address, DynPrecompile)],
) where
    DB: DatabaseRef,
    DB::Error: Send + Sync + 'static,
{
    for (address, precompile) in custom_precompiles {
        let precompile = precompile.clone();
        evm.precompiles.apply_precompile(address, move |_| Some(precompile));
    }
}

/// Mainnet handler with beneficiary rewards deferred to the commit thread.
struct NoRewardHandler<EVM, ERROR, FRAME> {
    _phantom: core::marker::PhantomData<(EVM, ERROR, FRAME)>,
}

impl<EVM, ERROR, FRAME> Default for NoRewardHandler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self { _phantom: core::marker::PhantomData }
    }
}

impl<EVM, ERROR, FRAME> Handler for NoRewardHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context: ContextTr<Journal: JournalTr<State = EvmState>>, Frame = FRAME>,
    ERROR: EvmTrError<EVM>,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    type Evm = EVM;
    type Error = ERROR;
    type HaltReason = HaltReason;

    fn reward_beneficiary(
        &self,
        _evm: &mut Self::Evm,
        _exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}
