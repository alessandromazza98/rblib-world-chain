use {
	alloy_evm::block::StateDB,
	alloy_op_evm::{
		OpBlockExecutionCtx,
		OpBlockExecutor,
		OpBlockExecutorFactory,
		OpEvmFactory,
		block::{OpTxEnv, receipt_builder::OpReceiptBuilder},
	},
	rblib::{
		alloy::{
			consensus::{Transaction, TxReceipt},
			eips::Encodable2718,
		},
		reth::{
			errors::BlockExecutionError,
			evm::{
				Database,
				Evm,
				EvmFactory,
				FromRecoveredTx,
				FromTxWithEncoded,
				OnStateHook,
				block::{
					BlockExecutor,
					BlockExecutorFactory,
					BlockExecutorFor,
					CommitChanges,
					ExecutableTx,
				},
			},
			optimism::{
				chainspec::OpChainSpec,
				evm::OpRethReceiptBuilder,
				forks::OpHardforks,
				primitives::{OpReceipt, OpTransactionSigned},
			},
			provider::BlockExecutionResult,
			revm::{
				DatabaseCommit,
				Inspector,
				State,
				context::result::{ExecutionResult, ResultAndState},
			},
		},
		revm::database::BundleState,
	},
};

/// A Block Executor for Optimism that can load pre state from previous
/// flashblocks.
pub struct FlashblocksBlockExecutor<Evm, R: OpReceiptBuilder, Spec> {
	inner: OpBlockExecutor<Evm, R, Spec>,
}

impl<'db, DB, E, R, Spec> FlashblocksBlockExecutor<E, R, Spec>
where
	DB: StateDB + Database + DatabaseCommit + 'db,
	E: Evm<
			DB = DB,
			Tx: FromRecoveredTx<R::Transaction>
			      + FromTxWithEncoded<R::Transaction>
			      + OpTxEnv,
		>,
	R: OpReceiptBuilder<
			Transaction: Transaction + Encodable2718,
			Receipt: TxReceipt,
		>,
	Spec: OpHardforks + Clone,
{
	/// Creates a new [`OpBlockExecutor`].
	pub fn new(
		evm: E,
		ctx: OpBlockExecutionCtx,
		spec: Spec,
		receipt_builder: R,
	) -> Self {
		let inner = OpBlockExecutor::new(evm, ctx, spec, receipt_builder);
		Self { inner }
	}

	/// Extends the [`BundleState`] of the executor with a specified pre-image.
	///
	/// This should be used _only_ when initializing the executor
	pub fn with_bundle_prestate(mut self, pre_state: BundleState) -> Self {
		self.evm_mut().db_mut().bundle_state_mut().extend(pre_state);
		self
	}

	/// Extends the receipts to reflect the aggregated execution result
	pub fn with_receipts(mut self, receipts: Vec<R::Receipt>) -> Self {
		self.inner.receipts.extend_from_slice(&receipts);
		self
	}

	/// Extends the gas used to reflect the aggregated execution result
	pub fn with_gas_used(mut self, gas_used: u64) -> Self {
		self.inner.gas_used += gas_used;
		self
	}
}

impl<'db, DB, E, R, Spec> BlockExecutor for FlashblocksBlockExecutor<E, R, Spec>
where
	DB: StateDB + Database + DatabaseCommit + 'db,
	E: Evm<
			DB = DB,
			Tx: FromRecoveredTx<R::Transaction>
			      + FromTxWithEncoded<R::Transaction>
			      + OpTxEnv,
		>,
	R: OpReceiptBuilder<
			Transaction: Transaction + Encodable2718,
			Receipt: TxReceipt,
		>,
	Spec: OpHardforks,
{
	type Evm = E;
	type Receipt = R::Receipt;
	type Transaction = R::Transaction;

	fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
		self.inner.apply_pre_execution_changes()
	}

	fn execute_transaction_with_commit_condition(
		&mut self,
		tx: impl ExecutableTx<Self>,
		f: impl FnOnce(
			&ExecutionResult<<Self::Evm as Evm>::HaltReason>,
		) -> CommitChanges,
	) -> Result<Option<u64>, BlockExecutionError> {
		self.inner.execute_transaction_with_commit_condition(tx, f)
	}

	fn execute_transaction_without_commit(
		&mut self,
		tx: impl ExecutableTx<Self>,
	) -> Result<ResultAndState<<Self::Evm as Evm>::HaltReason>, BlockExecutionError>
	{
		self.inner.execute_transaction_without_commit(tx)
	}

	fn commit_transaction(
		&mut self,
		output: ResultAndState<<Self::Evm as Evm>::HaltReason>,
		tx: impl ExecutableTx<Self>,
	) -> Result<u64, BlockExecutionError> {
		self.inner.commit_transaction(output, tx)
	}

	fn finish(
		self,
	) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError>
	{
		self.inner.finish()
	}

	fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
		self.inner.set_state_hook(hook)
	}

	fn evm_mut(&mut self) -> &mut Self::Evm {
		&mut self.inner.evm
	}

	fn evm(&self) -> &Self::Evm {
		self.inner.evm()
	}
}

/// Ethereum block executor factory.
#[derive(Debug, Clone)]
pub struct FlashblocksBlockExecutorFactory {
	inner: OpBlockExecutorFactory<OpRethReceiptBuilder, OpChainSpec>,
	pre_state: Option<BundleState>,
}

impl FlashblocksBlockExecutorFactory {
	/// Creates a new [`OpBlockExecutorFactory`] with the given spec,
	/// [`EvmFactory`], and [`OpReceiptBuilder`].
	pub const fn new(
		receipt_builder: OpRethReceiptBuilder,
		spec: OpChainSpec,
		evm_factory: OpEvmFactory,
	) -> Self {
		Self {
			inner: OpBlockExecutorFactory::new(receipt_builder, spec, evm_factory),
			pre_state: None,
		}
	}

	/// Exposes the chain specification.
	pub const fn spec(&self) -> &OpChainSpec {
		self.inner.spec()
	}

	/// Exposes the EVM factory.
	pub const fn evm_factory(&self) -> &OpEvmFactory {
		self.inner.evm_factory()
	}

	pub const fn take_bundle(&mut self) -> Option<BundleState> {
		self.pre_state.take()
	}

	/// Sets the pre-state for the block executor factory.
	pub fn set_pre_state(&mut self, pre_state: BundleState) {
		self.pre_state = Some(pre_state);
	}
}

impl BlockExecutorFactory for FlashblocksBlockExecutorFactory {
	type EvmFactory = OpEvmFactory;
	type ExecutionCtx<'a> = OpBlockExecutionCtx;
	type Receipt = OpReceipt;
	type Transaction = OpTransactionSigned;

	fn evm_factory(&self) -> &Self::EvmFactory {
		self.inner.evm_factory()
	}

	fn create_executor<'a, DB, I>(
		&'a self,
		evm: <OpEvmFactory as EvmFactory>::Evm<DB, I>,
		ctx: Self::ExecutionCtx<'a>,
	) -> impl BlockExecutorFor<'a, Self, DB, I>
	where
		DB: StateDB + Database + DatabaseCommit + 'a,
		I: Inspector<<OpEvmFactory as EvmFactory>::Context<DB>> + 'a,
	{
		if let Some(pre_state) = &self.pre_state {
			return FlashblocksBlockExecutor::new(
				evm,
				ctx,
				self.spec().clone(),
				OpRethReceiptBuilder::default(),
			)
			.with_bundle_prestate(pre_state.clone()); // TODO: Terrible clone here
		}

		FlashblocksBlockExecutor::new(
			evm,
			ctx,
			self.spec().clone(),
			OpRethReceiptBuilder::default(),
		)
	}
}
