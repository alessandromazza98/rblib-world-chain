use {
	crate::flashblocks::p2p::FlashblocksHandle,
	flashblocks_primitives::{
		flashblocks::{Flashblock, Flashblocks},
		primitives::FlashblocksPayloadV1,
	},
	parking_lot::RwLock,
	rblib::reth::{
		api::Events,
		optimism::{
			node::{OpBuiltPayload, OpEngineTypes, payload::config::OpBuilderConfig},
			primitives::OpPrimitives,
		},
	},
	reth_chain_state::ExecutedBlock,
	std::sync::Arc,
	tokio::sync::broadcast,
};

/// The current state of all known pre confirmations received over the P2P layer
/// or generated from the payload building job of this node.
///
/// The state is flushed when FCU is received with a parent hash that matches
/// the block hash of the latest pre confirmation _or_ when an FCU is received
/// that does not match the latest pre confirmation, in which case the pre
/// confirmations were not included as part of the canonical chain.
#[derive(Debug, Clone)]
pub struct FlashblocksStateExecutor {
	inner: Arc<RwLock<FlashblocksStateExecutorInner>>,
	p2p_handle: FlashblocksHandle,
	builder_config: OpBuilderConfig,
	pending_block:
		tokio::sync::watch::Sender<Option<ExecutedBlock<OpPrimitives>>>,
}

#[derive(Debug, Clone)]
pub struct FlashblocksStateExecutorInner {
	/// List of flashblocks for the current payload
	flashblocks: Flashblocks,
	/// The latest built payload with its associated flashblock index
	latest_payload: Option<(OpBuiltPayload, u64)>,
	payload_events: Option<broadcast::Sender<Events<OpEngineTypes>>>,
}

impl FlashblocksStateExecutor {
	/// Creates a new instance of [`FlashblocksStateExecutor`].
	///
	/// This function spawn a task that handles updates. It should only be called
	/// once.
	pub fn new(
		p2p_handle: FlashblocksHandle,
		builder_config: OpBuilderConfig,
		pending_block: tokio::sync::watch::Sender<
			Option<ExecutedBlock<OpPrimitives>>,
		>,
	) -> Self {
		let inner = Arc::new(RwLock::new(FlashblocksStateExecutorInner {
			flashblocks: Default::default(),
			latest_payload: None,
			payload_events: None,
		}));

		Self {
			inner,
			p2p_handle,
			builder_config,
			pending_block,
		}
	}

	pub fn publish_built_payload(
		&self,
		payload: FlashblocksPayloadV1,
		built_payload: OpBuiltPayload,
	) -> eyre::Result<()> {
		let FlashblocksStateExecutorInner {
			ref mut flashblocks,
			ref mut latest_payload,
			..
		} = *self.inner.write();

		let index = payload.index;
		let flashblock = Flashblock {
			flashblock: payload.clone(),
		};
		flashblocks.push(flashblock.clone())?;

		*latest_payload = Some((built_payload, index));

		self.p2p_handle.publish_new(payload);

		Ok(())
	}

	/// Returns a reference to the latest flashblock.
	pub fn last(&self) -> Flashblock {
		self.inner.read().flashblocks.last().clone()
	}

	/// Returns the entire flashblocks list for the current payload id.
	pub fn flashblocks(&self) -> Flashblocks {
		self.inner.read().flashblocks.clone()
	}

	/// Returns a receiver for the pending block.
	pub fn pending_block(
		&self,
	) -> tokio::sync::watch::Receiver<Option<ExecutedBlock<OpPrimitives>>> {
		self.pending_block.subscribe()
	}
}
