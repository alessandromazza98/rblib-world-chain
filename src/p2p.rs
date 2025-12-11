use {
	crate::flashblocks::FlashblocksStateExecutor,
	flashblocks_primitives::primitives::FlashblocksPayloadV1,
	rblib::reth::optimism::node::OpBuiltPayload,
};

#[derive(Debug)]
pub struct FlashblocksP2p {
	pub flashblocks_state: FlashblocksStateExecutor,
}

impl FlashblocksP2p {
	pub fn new(flashblocks_state: FlashblocksStateExecutor) -> Self {
		Self { flashblocks_state }
	}

	pub fn publish(
		&self,
		payload: FlashblocksPayloadV1,
		built_payload: OpBuiltPayload,
	) -> eyre::Result<()> {
		self
			.flashblocks_state
			.publish_built_payload(payload, built_payload)
	}
}
