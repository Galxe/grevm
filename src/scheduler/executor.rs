//! EVM construction and transaction driving for the scheduler.

use crate::{
    TxVersion,
    cache_db::CacheDB,
    delegated_safety::{
        BeneficiaryMode, DelegatedSafetyConfig, GrevmHandler, ReserveMode, ReservePlanner,
        gravity_instructions,
    },
};
use alloy_evm::{
    Database as AlloyDatabase,
    precompiles::{DynPrecompile, PrecompilesMap},
};
use revm::{
    Context, DatabaseRef, ExecuteEvm, MainBuilder, MainContext,
    context::Evm as RevmEvm,
    handler::{EthFrame, instructions::EthInstructions},
    interpreter::interpreter::EthInterpreter,
    precompile::{PrecompileSpecId, Precompiles},
};
use revm_context::{
    BlockEnv, CfgEnv, ContextSetters, ContextTr, TxEnv,
    result::{EVMError, ExecutionResult, ResultAndState},
};
use revm_inspector::NoOpInspector;
use revm_primitives::Address;
use std::{fmt::Debug, sync::Arc};

type GrevmContext<DB> = Context<BlockEnv, TxEnv, CfgEnv, DB>;

pub(crate) type GrevmEvm<DB> = RevmEvm<
    GrevmContext<DB>,
    NoOpInspector,
    EthInstructions<EthInterpreter, GrevmContext<DB>>,
    PrecompilesMap,
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

    fn db_mut(&mut self) -> &mut CacheDB<'db, DB>;
}

/// One executor for all four delegated-safety configurations.
///
/// CREATE protection is selected once in the instruction table. Reserve protection is an optional
/// handler around the same upstream journal and precompile provider.
pub(crate) struct GrevmExecutor<'a, DB>
where
    DB: DatabaseRef,
{
    evm: GrevmEvm<CacheDB<'a, DB>>,
    /// `Some` enables delegated-balance reserve checks; `None` adds no checkpoint or scan.
    reserve_planner: Option<Arc<ReservePlanner>>,
}

impl<'a, DB> GrevmExecutor<'a, DB>
where
    DB: DatabaseRef + Debug,
    DB::Error: Send + Sync + 'static,
{
    pub(crate) fn new(
        db: CacheDB<'a, DB>,
        cfg: CfgEnv,
        block: BlockEnv,
        custom_precompiles: &[(Address, DynPrecompile)],
        safety: DelegatedSafetyConfig,
        reserve_planner: Option<Arc<ReservePlanner>>,
    ) -> Self {
        let evm = build_evm(db, cfg, block, custom_precompiles, safety.forbid_delegated_create);
        debug_assert_eq!(safety.reserve_delegated_balance, reserve_planner.is_some());
        Self { evm, reserve_planner }
    }
}

impl<'a, DB> ParallelTransactionExecutor<'a, DB> for GrevmExecutor<'a, DB>
where
    DB: DatabaseRef + Debug,
    DB::Error: Send + Sync + 'static,
{
    fn transact(
        &mut self,
        version: TxVersion,
        tx: TxEnv,
    ) -> Result<ResultAndState, EVMError<DB::Error>> {
        self.evm.db_mut().reset_state(version.clone());
        self.evm.ctx.set_tx(tx);
        let reserve_mode = ReserveMode::from_planner(version.txid, self.reserve_planner.as_deref());
        let output: Result<ExecutionResult, EVMError<DB::Error>> =
            GrevmHandler::new(reserve_mode, BeneficiaryMode::Deferred).run(&mut self.evm);
        let state = self.evm.finalize();
        output.map(|result| ResultAndState { result, state })
    }

    fn db_mut(&mut self) -> &mut CacheDB<'a, DB> {
        self.evm.db_mut()
    }
}

pub(crate) fn build_evm<DB>(
    db: DB,
    cfg: CfgEnv,
    block: BlockEnv,
    custom_precompiles: &[(Address, DynPrecompile)],
    forbid_delegated_create: bool,
) -> GrevmEvm<DB>
where
    DB: AlloyDatabase,
{
    let spec = cfg.spec;
    let mut evm = Context::mainnet()
        .with_db(db)
        .with_cfg(cfg)
        .with_block(block)
        .build_mainnet_with_inspector(NoOpInspector {})
        .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
            PrecompileSpecId::from_spec_id(spec),
        )));
    // Keep this local gate as a defensive invariant for callers of this construction helper;
    // Scheduler also normalizes the complete delegated-safety policy for the selected spec.
    if forbid_delegated_create && spec.is_enabled_in(revm_primitives::hardfork::SpecId::PRAGUE) {
        evm.instruction = gravity_instructions(spec);
    }
    for (address, precompile) in custom_precompiles {
        let precompile = precompile.clone();
        evm.precompiles.apply_precompile(address, move |_| Some(precompile));
    }
    evm
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm_database::EmptyDB;
    use revm_primitives::hardfork::SpecId;

    #[test]
    fn delegated_create_guard_preserves_selected_spec_gas_table() {
        for spec in [
            SpecId::FRONTIER,
            SpecId::SPURIOUS_DRAGON,
            SpecId::BERLIN,
            SpecId::PRAGUE,
            SpecId::OSAKA,
            SpecId::AMSTERDAM,
        ] {
            let cfg = CfgEnv::new_with_spec(spec);
            let block = BlockEnv::default();
            let standard = build_evm(EmptyDB::new(), cfg.clone(), block.clone(), &[], false);
            let guarded = build_evm(EmptyDB::new(), cfg, block, &[], true);

            assert_eq!(
                guarded.instruction.gas_table(),
                standard.instruction.gas_table(),
                "gas table mismatch for {spec:?}"
            );
        }
    }
}
