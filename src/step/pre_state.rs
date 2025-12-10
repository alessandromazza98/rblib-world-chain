use {
	crate::{WorldChain, context::WorldContext},
	flashblocks_builder::executor::FlashblocksStateExecutor,
	flashblocks_primitives::flashblocks::Flashblock,
	rblib::{
		alloy::optimism::consensus::OpTxEnvelope,
		prelude::{
			BlockExt,
			Checkpoint,
			ControlFlow,
			PayloadBuilderError,
			Step,
			StepContext,
		},
		reth::{
			optimism::{node::OpBuiltPayload, primitives::OpReceipt},
			payload::primitives::BuiltPayload,
			primitives::{Block, RecoveredBlock},
		},
	},
	std::sync::Arc,
};

#[derive(Debug, thiserror::Error)]
enum FetchPreStateError {
	#[error("Failed to convert flashblock to recovered block: {0}")]
	FlashblockConversion(eyre::Report),
}

#[derive(Debug, Clone)]
pub struct FetchPreState {
	/// The current state of all known pre confirmations received over the P2P
	/// layer or generated from the payload building job of this node.
	pub flashblocks_state: FlashblocksStateExecutor,
}

impl FetchPreState {
	/// Create a new `FetchPreState` with the provided flashblocks state.
	pub fn new(flashblocks_state: FlashblocksStateExecutor) -> Self {
		Self { flashblocks_state }
	}
}

impl Step<WorldChain> for FetchPreState {
	async fn step(
		self: Arc<Self>,
		payload: Checkpoint<WorldChain>,
		ctx: StepContext<WorldChain>,
	) -> ControlFlow<WorldChain> {
		let flashblocks = self.flashblocks_state.flashblocks();
		let maybe_flashblock = Flashblock::reduce(flashblocks);
		if let Some(flashblock) = maybe_flashblock {
			if *flashblock.payload_id() == ctx.block().payload_id().0 {
				// If we have a pre-confirmed state, we can use it to build the payload
				let block: RecoveredBlock<Block<OpTxEnvelope>> =
					match flashblock.clone().try_into() {
						Ok(block) => block,
						Err(err) => {
							return ControlFlow::Fail(PayloadBuilderError::other(
								FetchPreStateError::FlashblockConversion(err),
							));
						}
					};
				let sealed = block.into_sealed_block();
				let op_built_payload = OpBuiltPayload::new(
					ctx.block().payload_id(),
					Arc::new(sealed),
					flashblock.flashblock().metadata.fees,
					None,
				);
				let world_ctx = WorldContext::new(op_built_payload);
				let checkpoint = payload.barrier_with_context(world_ctx);
				return ControlFlow::Ok(checkpoint);
			}
		}
		ControlFlow::Ok(payload)
	}
}
