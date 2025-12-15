use {
	crate::flashblocks::block_executor::{
		FlashblocksBlockExecutor,
		FlashblocksBlockExecutorFactory,
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

		// flatten reverts into a single reverts as the bundle is re-used across
		// multiple payloads which represent a single atomic state transition.
		// therefore reverts should have length 1 we only retain the first
		// occurance of a revert for any given account.
		let flattened = db
			.bundle_state
			.reverts
			.iter()
			.flatten()
			.scan(HashSet::new(), |visited, (acc, revert)| {
				if visited.insert(acc) {
					Some((*acc, revert.clone()))
				} else {
					None
				}
			})
			.collect();

		db.bundle_state.reverts = Reverts::new(vec![flattened]);

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
