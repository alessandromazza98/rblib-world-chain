use {
	crate::flashblocks::{
		ctx::OpPayloadBuilderCtxBuilder,
		p2p::FlashblocksHandle,
		primitives::{Flashblock, Flashblocks, FlashblocksPayloadV1},
	},
	futures::StreamExt,
	parking_lot::RwLock,
	rblib::{
		alloy::consensus,
		reth::{
			api::{Events, FullNodeTypes, NodeTypes, TxTy},
			builder::BuilderContext,
			optimism::{
				chainspec::OpChainSpec,
				evm::OpEvmConfig,
				node::{
					OpBuiltPayload,
					OpEngineTypes,
					payload::{builder::OpPayloadBuilderCtx, config::OpBuilderConfig},
				},
				primitives::OpPrimitives,
			},
			provider::{HeaderProvider, StateProviderFactory},
			transaction_pool::{PoolTransaction, TransactionPool},
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

	/// Returns the associated `FlashblocksHandle`.
	pub fn p2p_handle(&self) -> FlashblocksHandle {
		self.p2p_handle.clone()
	}

	/// Returns a reference to the op builder config.
	pub fn builder_config(&self) -> &OpBuilderConfig {
		&self.builder_config
	}

	/// Launches the executor to listen for new flashblocks and build payloads.
	pub fn launch<Node, Pool>(
		&self,
		ctx: &BuilderContext<Node>,
		pool: Pool,
		// TODO: use `WorldChainPayloadBuilderCtx` type
		payload_builder_ctx_builder: OpPayloadBuilderCtxBuilder,
		evm_config: OpEvmConfig,
	) where
		Pool: TransactionPool<
				Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>,
			> + Unpin
			+ 'static,
		// Node::Provider:
		// 	StateProviderFactory + HeaderProvider<Header = consensus::Header>,
		Node: FullNodeTypes<Types: NodeTypes<ChainSpec = OpChainSpec>>,
	{
		let mut stream = self.p2p_handle.flashblock_stream();
		let this = self.clone();
		let provider = ctx.provider().clone();
		let chain_spec = ctx.chain_spec().clone();

		let pending_block = self.pending_block.clone();

		ctx
			.task_executor()
			.spawn_critical("flashblocks executor", async move {
				while let Some(flashblock) = stream.next().await {
					if let Err(e) = process_flashblock(
						&provider,
						&pool,
						&payload_builder_ctx_builder,
						&evm_config,
						&this,
						&chain_spec,
						flashblock,
						pending_block.clone(),
					) {
						tracing::error!("error processing flashblock: {e:?}")
					}
				}
			});
	}
}

