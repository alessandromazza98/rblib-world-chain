use {
	alloy_op_evm::OpBlockExecutionCtx,
	rblib::{
		alloy::consensus::Block,
		reth::{
			core::primitives::{Receipt, SignedTransaction},
			errors::BlockExecutionError,
			evm::{
				block::BlockExecutorFactory,
				execute::{BlockAssembler, BlockAssemblerInput},
			},
			optimism::{
				chainspec::OpChainSpec,
				evm::OpBlockAssembler,
				primitives::DepositReceipt,
			},
		},
	},
	std::sync::Arc,
};

/// Block builder for Optimism.
#[derive(Debug)]
pub struct FlashblocksBlockAssembler {
	inner: OpBlockAssembler<OpChainSpec>,
}

impl FlashblocksBlockAssembler {
	/// Creates a new [`OpBlockAssembler`].
	pub const fn new(chain_spec: Arc<OpChainSpec>) -> Self {
		Self {
			inner: OpBlockAssembler::new(chain_spec),
		}
	}
}

impl FlashblocksBlockAssembler {
	/// Builds a block for `input` without any bounds on header `H`.
	pub fn assemble_block<
		F: for<'a> BlockExecutorFactory<
				ExecutionCtx<'a> = OpBlockExecutionCtx,
				Transaction: SignedTransaction,
				Receipt: Receipt + DepositReceipt,
			>,
		H,
	>(
		&self,
		input: BlockAssemblerInput<'_, '_, F, H>,
	) -> Result<Block<F::Transaction>, BlockExecutionError> {
		self.inner.assemble_block(input)
	}
}

impl Clone for FlashblocksBlockAssembler {
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
		}
	}
}

impl<F> BlockAssembler<F> for FlashblocksBlockAssembler
where
	F: for<'a> BlockExecutorFactory<
			ExecutionCtx<'a> = OpBlockExecutionCtx,
			Transaction: SignedTransaction,
			Receipt: Receipt + DepositReceipt,
		>,
{
	type Block = Block<F::Transaction>;

	fn assemble_block(
		&self,
		input: BlockAssemblerInput<'_, '_, F>,
	) -> Result<Self::Block, BlockExecutionError> {
		self.assemble_block(input)
	}
}
