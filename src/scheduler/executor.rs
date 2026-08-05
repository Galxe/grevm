//! EVM construction and transaction driving for the scheduler.

use crate::{
    DynParallelPrecompile, TxVersion,
    beneficiary::{BeneficiaryMode, SpeculativeResult},
    delegated_safety::{
        DelegatedSafetyConfig, GrevmHandler, ReserveMode, ReservePlanner, gravity_instructions,
    },
    incarnation_db::{IncarnationAccesses, IncarnationDb},
};
use alloy_evm::{Database as AlloyDatabase, precompiles::PrecompilesMap};
use revm::{
    Context, DatabaseRef, ExecuteEvm, MainBuilder, MainContext,
    context::Evm as RevmEvm,
    handler::{EthFrame, instructions::EthInstructions},
    interpreter::interpreter::EthInterpreter,
    precompile::{PrecompileSpecId, Precompiles},
};
use revm_context::{BlockEnv, CfgEnv, ContextSetters, ContextTr, TxEnv, result::EVMError};
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
pub(crate) trait ParallelTransactionExecutor<DB>
where
    DB: DatabaseRef,
{
    fn execute_incarnation(
        &mut self,
        version: TxVersion,
        tx: TxEnv,
    ) -> IncarnationExecution<DB::Error>;
}

/// EVM outcome and access metadata produced by one complete incarnation lifecycle.
pub(crate) struct IncarnationExecution<DBError> {
    pub(crate) result: Result<SpeculativeResult, EVMError<DBError>>,
    pub(crate) accesses: IncarnationAccesses,
}

/// One executor for all four delegated-safety configurations.
///
/// CREATE protection is selected once in the instruction table. Reserve protection is an optional
/// handler around the same upstream journal and precompile provider.
pub(crate) struct GrevmExecutor<'a, DB>
where
    DB: DatabaseRef,
{
    evm: GrevmEvm<IncarnationDb<'a, DB>>,
    /// `Some` enables delegated-balance reserve checks; `None` adds no checkpoint or scan.
    reserve_planner: Option<Arc<ReservePlanner>>,
}

impl<'a, DB> GrevmExecutor<'a, DB>
where
    DB: DatabaseRef + Debug,
    DB::Error: Send + Sync + 'static,
{
    pub(crate) fn new(
        incarnation_db: IncarnationDb<'a, DB>,
        cfg: CfgEnv,
        block: BlockEnv,
        custom_precompiles: &[(Address, DynParallelPrecompile)],
        safety: DelegatedSafetyConfig,
        reserve_planner: Option<Arc<ReservePlanner>>,
    ) -> Self {
        assert!(
            incarnation_db.beneficiary_matches(block.beneficiary),
            "executor and incarnation database must use the same block beneficiary",
        );
        let evm = build_evm(
            incarnation_db,
            cfg,
            block,
            custom_precompiles,
            safety.forbid_delegated_create,
        );
        debug_assert_eq!(safety.reserve_delegated_balance, reserve_planner.is_some());
        Self { evm, reserve_planner }
    }
}

impl<'a, DB> ParallelTransactionExecutor<DB> for GrevmExecutor<'a, DB>
where
    DB: DatabaseRef + Debug,
    DB::Error: Send + Sync + 'static,
{
    fn execute_incarnation(
        &mut self,
        version: TxVersion,
        tx: TxEnv,
    ) -> IncarnationExecution<DB::Error> {
        let txid = version.txid;
        self.evm.db_mut().begin_incarnation(version);
        self.evm.ctx.set_tx(tx);
        let reserve_mode = ReserveMode::from_planner(txid, self.reserve_planner.as_deref());
        let output = GrevmHandler::new(reserve_mode, BeneficiaryMode::Deferred).run(&mut self.evm);
        let state = self.evm.finalize();
        let result = output.map(|output| output.into_speculative(state));
        let accesses = match &result {
            Ok(result) => self.evm.db_mut().finish_incarnation(result.state()),
            Err(_) => self.evm.db_mut().discard_incarnation(),
        };
        IncarnationExecution { result, accesses }
    }
}

pub(crate) fn build_evm<DB>(
    db: DB,
    cfg: CfgEnv,
    block: BlockEnv,
    custom_precompiles: &[(Address, DynParallelPrecompile)],
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
        let precompile = precompile.to_alloy();
        evm.precompiles.apply_precompile(address, move |_| Some(precompile));
    }
    evm
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MVMemory, beneficiary::Beneficiary};
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

    #[test]
    #[should_panic(
        expected = "executor and incarnation database must use the same block beneficiary"
    )]
    fn executor_rejects_incarnation_db_for_another_beneficiary() {
        let backing_db = EmptyDB::default();
        let memory = MVMemory::default();
        let beneficiary = Beneficiary::new(Address::with_last_byte(1), None, 1);
        let incarnation_db = IncarnationDb::new(&backing_db, &memory, &beneficiary);
        let block = BlockEnv { beneficiary: Address::with_last_byte(2), ..Default::default() };

        let _ = GrevmExecutor::new(
            incarnation_db,
            CfgEnv::default(),
            block,
            &[],
            DelegatedSafetyConfig::disabled(),
            None,
        );
    }
}
