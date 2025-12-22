use {
	crate::flashblocks::{
		builder::FlashblockBuilder,
		ctx::OpPayloadBuilderCtxBuilder,
		p2p::FlashblocksHandle,
		primitives::{Flashblock, Flashblocks, FlashblocksPayloadV1},
	},
	backon::{BlockingRetryable, ExponentialBuilder},
	eyre::OptionExt,
	futures::StreamExt,
	parking_lot::RwLock,
	rblib::{
		alloy::{
			eips::{Decodable2718, eip2718::WithEncoded},
			optimism::consensus::{OpTxEnvelope, encode_holocene_extra_data},
			primitives::BlockHash,
		},
		reth::{
			api::{Events, FullNodeTypes, NodeTypes},
			builder::BuilderContext,
			optimism::{
				chainspec::OpChainSpec,
				evm::OpEvmConfig,
				node::{
					OpBuiltPayload,
					OpEngineTypes,
					OpPayloadBuilderAttributes,
					payload::config::OpBuilderConfig,
				},
				primitives::OpPrimitives,
				txpool::OpPooledTx,
			},
			payload::{
				BuiltPayload,
				EthPayloadBuilderAttributes,
				PayloadId,
				builder::{BuildOutcomeKind, PayloadConfig},
				util::BestPayloadTransactions,
			},
			primitives::{Header, SealedHeader},
			provider::{ChainSpecProvider, HeaderProvider, StateProviderFactory},
			revm::{cancelled::CancelOnDrop, database::StateProviderDatabase},
			rpc::types::Withdrawals,
			transaction_pool::{
				PoolTransaction,
				TransactionPool,
				ValidPoolTransaction,
			},
		},
	},
	reth_chain_state::ExecutedBlock,
	std::{sync::Arc, time::Duration},
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

	/// Publishes a newly built payload to the P2P network and updates internal
	/// state.
	///
	/// This method is called when this node has built a new flashblock that
	/// should be broadcast to peers.
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
			flashblock: payload,
		};
		flashblocks.push(flashblock.clone())?;

		*latest_payload = Some((built_payload, index));

		// Broadcast to P2P network using the flashblock's inner payload
		self.p2p_handle.publish_new(flashblock.into_flashblock());

		Ok(())
	}

	/// Registers a new broadcast channel for built payloads.
	pub fn register_payload_events(
		&self,
		tx: broadcast::Sender<Events<OpEngineTypes>>,
	) {
		self.inner.write().payload_events = Some(tx);
	}

	/// Broadcasts a new payload to cache in the in memory tree.
	pub fn broadcast_payload(
		&self,
		event: Events<OpEngineTypes>,
		payload_events: Option<broadcast::Sender<Events<OpEngineTypes>>>,
	) -> eyre::Result<()> {
		if let Some(payload_events) = payload_events {
			if let Err(e) = payload_events.send(event) {
				tracing::error!("error broadcasting payload: {e:?}");
			}
		}
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
				Transaction: PoolTransaction<Consensus = OpTxEnvelope> + OpPooledTx,
			> + Unpin
			+ 'static,
		Node::Provider: StateProviderFactory + HeaderProvider<Header = Header>,
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

/// Processes a received flashblock: validates it, builds the payload, and
/// updates state.
///
/// # Processing Steps
/// 1. Check if flashblock was already processed (skip if so)
/// 2. Determine if this is a new payload or continuation of existing one
/// 3. Decode and validate transactions from the flashblock
/// 4. Build the payload using the flashblock builder
/// 5. Update internal state and broadcast the result
#[expect(clippy::too_many_arguments)]
fn process_flashblock<Provider, Pool>(
	provider: &Provider,
	pool: &Pool,
	payload_builder_ctx_builder: &OpPayloadBuilderCtxBuilder,
	evm_config: &OpEvmConfig,
	state_executor: &FlashblocksStateExecutor,
	chain_spec: &Arc<OpChainSpec>,
	flashblock: FlashblocksPayloadV1,
	pending_block: tokio::sync::watch::Sender<
		Option<ExecutedBlock<OpPrimitives>>,
	>,
) -> eyre::Result<()>
where
	Provider: StateProviderFactory
		+ HeaderProvider<Header = Header>
		+ ChainSpecProvider<ChainSpec = OpChainSpec>
		+ Clone,
	Pool: TransactionPool<
			Transaction: PoolTransaction<Consensus = OpTxEnvelope> + OpPooledTx,
		> + 'static,
{
	tracing::trace!(target: "flashblocks::state_executor",id = %flashblock.payload_id, index = %flashblock.index, "processing flashblock");

	let FlashblocksStateExecutorInner {
		ref mut flashblocks,
		ref mut latest_payload,
		ref mut payload_events,
	} = *state_executor.inner.write();

	let flashblock = Flashblock { flashblock };

	if let Some(latest_payload) = latest_payload {
		if latest_payload.0.id() == flashblock.flashblock.payload_id
			&& latest_payload.1 >= flashblock.flashblock.index
		{
			// Already processed this flashblock. This happens when set directly
			// from publish_build_payload. Since we already built the payload, no need
			// to do it again.
			pending_block.send_replace(latest_payload.0.executed_block());
			return Ok(());
		}
	}

	// If for whatever reason we are not processing flashblocks in order
	// we will error and return here.
	let base = if flashblocks.is_new_payload(&flashblock)? {
		*latest_payload = None;
		// safe unwrap from check in is_new_payload
		flashblock.base().unwrap()
	} else {
		flashblocks.base()
	};

	let diff = &flashblock.flashblock.diff;
	let index = flashblock.flashblock.index;
	let cancel = CancelOnDrop::default();

	let transactions = diff
		.transactions
		.iter()
		.map(|b| {
			let tx: OpTxEnvelope = Decodable2718::decode_2718_exact(b)?;
			eyre::Result::Ok(WithEncoded::new(b.clone(), tx))
		})
		.collect::<eyre::Result<Vec<_>>>()?;

	let eth_attrs = EthPayloadBuilderAttributes {
		id: PayloadId(flashblock.payload_id().to_owned()),
		parent: base.parent_hash,
		timestamp: base.timestamp,
		suggested_fee_recipient: base.fee_recipient,
		prev_randao: base.prev_randao,
		withdrawals: Withdrawals(diff.withdrawals.clone()),
		parent_beacon_block_root: Some(base.parent_beacon_block_root),
	};

	let eip1559 = encode_holocene_extra_data(
		Default::default(),
		chain_spec.base_fee_params_at_timestamp(base.timestamp),
	)?;

	let attributes = OpPayloadBuilderAttributes {
		payload_attributes: eth_attrs,
		no_tx_pool: true,
		transactions: transactions.clone(),
		gas_limit: None,
		eip_1559_params: Some(eip1559[1..=8].try_into()?),
		min_base_fee: None,
	};

	let sealed_header = (|| fetch_header(provider, base.parent_hash))
		.retry(
			ExponentialBuilder::default()
				.with_min_delay(Duration::from_millis(MIN_BACKOFF_DELAY))
				.with_max_delay(Duration::from_millis(MAX_BACKOFF_DELAY))
				.without_max_times(),
		)
		.sleep(std::thread::sleep)
		.call()?;

	let state_provider = provider.state_by_block_hash(base.parent_hash)?;

	let config = PayloadConfig::new(Arc::new(sealed_header), attributes);
	let builder_ctx = payload_builder_ctx_builder.build(
		provider.clone(),
		evm_config.clone(),
		state_executor.builder_config.clone(),
		config,
		&cancel,
		latest_payload.as_ref().map(|p| p.0.clone()),
	);

	let best = |_| {
		// The empty vec needs an explicit tx type, otherwise Rust can't infer the
		// `PoolTransaction` used by `BestPayloadTransactions`.
		let empty: Vec<Arc<ValidPoolTransaction<Pool::Transaction>>> = Vec::new();
		BestPayloadTransactions::<Pool::Transaction, _>::new(empty.into_iter())
	};
	let db = StateProviderDatabase::new(&state_provider);

	let outcome = FlashblockBuilder::new(best).build(
		pool.clone(),
		db,
		&state_provider,
		&builder_ctx,
		latest_payload.as_ref().map(|p| p.0.clone()),
	)?;

	let payload = match outcome {
		BuildOutcomeKind::Better { payload } => payload,
		BuildOutcomeKind::Freeze(payload) => payload,
		_ => return Ok(()),
	};

	debug_assert_eq!(
		payload.block().hash(),
		flashblock.diff().block_hash,
		"executed block hash does not match flashblock diff block hash"
	);

	tracing::trace!(target: "flashblocks::state_executor", hash = %payload.block().hash(), "setting latest payload");
	flashblocks.push(flashblock)?;
	*latest_payload = Some((payload.clone(), index));
	pending_block.send_replace(payload.executed_block());

	state_executor.broadcast_payload(
		Events::BuiltPayload(payload.clone()),
		payload_events.clone(),
	)?;

	Ok(())
}

const MIN_BACKOFF_DELAY: u64 = 20;
const MAX_BACKOFF_DELAY: u64 = 200;

/// Fetch the sealed header of the provided block hash.
fn fetch_header<P: HeaderProvider<Header = Header>>(
	provider: &P,
	block_hash: BlockHash,
) -> eyre::Result<SealedHeader> {
	provider
		.sealed_header_by_hash(block_hash)?
		.ok_or_eyre(format!("missing sealed header: {}", block_hash))
}
