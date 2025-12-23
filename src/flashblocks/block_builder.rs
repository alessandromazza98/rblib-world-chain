use {
	crate::{
		flashblocks::block_executor::{
			FlashblocksBlockExecutor,
			FlashblocksBlockExecutorFactory,
		},
		step::publish_flashblocks::flatten_reverts,
	},
	alloy_op_evm::block::OpTxEnv,
	rblib::{
		alloy::consensus::Block,
		reth::{
			api::NodePrimitives,
			errors::BlockExecutionError,
			evm::{
				Database,
				Evm,
				FromRecoveredTx,
				FromTxWithEncoded,
				block::{BlockExecutor, CommitChanges},
				execute::{
					BasicBlockBuilder,
					BlockAssemblerInput,
					BlockBuilder,
					BlockBuilderOutcome,
					ExecutorTx,
				},
				op_revm::{OpHaltReason, OpSpecId},
			},
			optimism::{
				chainspec::OpChainSpec,
				evm::{OpBlockAssembler, OpBlockExecutionCtx, OpRethReceiptBuilder},
				primitives::{OpReceipt, OpTransactionSigned},
			},
			primitives::{Header, Recovered, RecoveredBlock, SealedHeader},
			provider::StateProvider,
			revm::{
				State,
				context::{BlockEnv, result::ExecutionResult},
			},
		},
		revm::database::states::{bundle_state::BundleRetention, reverts::Reverts},
	},
	std::{collections::HashSet, sync::Arc},
};

/// A wrapper around the [`BasicBlockBuilder`] for flashblocks.
pub struct FlashblocksBlockBuilder<'a, N: NodePrimitives, Evm> {
	pub inner: BasicBlockBuilder<
		'a,
		FlashblocksBlockExecutorFactory,
		FlashblocksBlockExecutor<Evm, OpRethReceiptBuilder, Arc<OpChainSpec>>,
		OpBlockAssembler<OpChainSpec>,
		N,
	>,
}

impl<'a, N: NodePrimitives, Evm> FlashblocksBlockBuilder<'a, N, Evm> {
	/// Creates a new [`FlashblocksBlockBuilder`] with the given executor factory
	/// and assembler.
	pub fn new(
		ctx: OpBlockExecutionCtx,
		parent: &'a SealedHeader<N::BlockHeader>,
		executor: FlashblocksBlockExecutor<
			Evm,
			OpRethReceiptBuilder,
			Arc<OpChainSpec>,
		>,
		transactions: Vec<Recovered<N::SignedTx>>,
		chain_spec: Arc<OpChainSpec>,
	) -> Self {
		Self {
			inner: BasicBlockBuilder {
				executor,
				assembler: OpBlockAssembler::new(chain_spec),
				ctx,
				parent,
				transactions,
			},
		}
	}
}

impl<'a, DB, N, E> BlockBuilder for FlashblocksBlockBuilder<'a, N, E>
where
	DB: Database + 'a,
	N: NodePrimitives<
			Receipt = OpReceipt,
			SignedTx = OpTransactionSigned,
			Block = Block<OpTransactionSigned>,
			BlockHeader = Header,
		>,
	E: Evm<
			DB = &'a mut State<DB>,
			Tx: FromRecoveredTx<OpTransactionSigned>
			      + FromTxWithEncoded<OpTransactionSigned>
			      + OpTxEnv,
			Spec = OpSpecId,
			HaltReason = OpHaltReason,
			BlockEnv = BlockEnv,
		>,
{
	type Executor =
		FlashblocksBlockExecutor<E, OpRethReceiptBuilder, Arc<OpChainSpec>>;
	type Primitives = N;

	fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
		self.inner.apply_pre_execution_changes()
	}

	fn execute_transaction_with_commit_condition(
		&mut self,
		tx: impl ExecutorTx<Self::Executor>,
		f: impl FnOnce(
			&ExecutionResult<
				<<Self::Executor as BlockExecutor>::Evm as Evm>::HaltReason,
			>,
		) -> CommitChanges,
	) -> Result<Option<u64>, BlockExecutionError> {
		self.inner.execute_transaction_with_commit_condition(tx, f)
	}

	fn finish(
		self,
		state: impl StateProvider,
	) -> Result<BlockBuilderOutcome<N>, BlockExecutionError> {
		let (evm, result) = self.inner.executor.finish()?;
		let (db, evm_env) = evm.finish();

		// merge all transitions into bundle state
		db.merge_transitions(BundleRetention::Reverts);

		// Flatten reverts into a single transition:
		// - per account: keep earliest `previous_status`
		// - per account: keep earliest non-`DoNothing` account-info revert
		// - per account+slot: keep earliest revert-to value
		// - per account: OR `wipe_storage`
		//
		// This keeps `bundle_state.reverts.len() == 1`, which matches the
		// expectation that this bundle represents a single block worth of changes
		// even if we built multiple payloads.
		let flattened = flatten_reverts(&db.bundle_state.reverts);
		db.bundle_state.reverts = flattened;

		// calculate the state root
		let hashed_state = state.hashed_post_state(&db.bundle_state);
		let (state_root, trie_updates) = state
			.state_root_with_updates(hashed_state.clone())
			.map_err(BlockExecutionError::other)?;

		let (transactions, senders) = self
			.inner
			.transactions
			.into_iter()
			.map(|tx| tx.into_parts())
			.unzip();

		let block = self.inner.assembler.assemble_block(BlockAssemblerInput::<
			'_,
			'_,
			FlashblocksBlockExecutorFactory,
		>::new(
			evm_env,
			self.inner.ctx,
			self.inner.parent,
			transactions,
			&result,
			&db.bundle_state,
			&state,
			state_root,
		))?;

		let block = RecoveredBlock::new_unhashed(block, senders);

		Ok(BlockBuilderOutcome {
			execution_result: result,
			hashed_state,
			trie_updates,
			block,
		})
	}

	fn executor_mut(&mut self) -> &mut Self::Executor {
		self.inner.executor_mut()
	}

	fn executor(&self) -> &Self::Executor {
		self.inner.executor()
	}

	fn into_executor(self) -> Self::Executor {
		self.inner.into_executor()
	}
}
