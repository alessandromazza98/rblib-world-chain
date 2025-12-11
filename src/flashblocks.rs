use {
	crate::flashblocks::primitives::FlashblocksP2PMsg,
	alloy_rlp::{BytesMut, Encodable},
	chrono::Utc,
	ed25519_dalek::{SigningKey, VerifyingKey},
	flashblocks_p2p::protocol::{
		error::FlashblocksP2PError,
		handler::{FlashblocksP2PState, PeerMsg},
	},
	flashblocks_primitives::{
		flashblocks::{Flashblock, Flashblocks},
		primitives::FlashblocksPayloadV1,
	},
	parking_lot::{Mutex, RwLock},
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

pub mod primitives;

/// Maximum frame size for rlpx messages.
const MAX_FRAME: usize = 1 << 24; // 16 MiB

/// Maximum index for flashblocks payloads.
/// Not intended to ever be hit. Since we resize the flashblocks vector
/// dynamically, this is just a sanity check to prevent excessive memory usage.
const MAX_FLASHBLOCK_INDEX: usize = 100;

/// The maximum number of broadcast channel messages we will buffer
/// before dropping them. In practice, we should rarely need to buffer any
/// messages.
const BROADCAST_BUFFER_CAPACITY: usize = 100;

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

/// Context struct containing shared resources for the flashblocks P2P protocol.
///
/// This struct holds the network handle, cryptographic keys, and communication
/// channels used across all connections in the flashblocks P2P protocol. It
/// provides the shared infrastructure needed for message verification, signing,
/// and broadcasting.
#[derive(Clone, Debug)]
pub struct FlashblocksP2PCtx {
	/// Authorizer's verifying key used to verify authorization signatures from
	/// rollup-boost.
	pub authorizer_vk: VerifyingKey,
	/// Builder's signing key used to sign outgoing authorized P2P messages.
	pub builder_sk: Option<SigningKey>,
	/// Broadcast sender for peer messages that will be sent to all connected
	/// peers. Messages may not be strictly ordered due to network conditions.
	pub peer_tx: broadcast::Sender<PeerMsg>,
	/// Broadcast sender for verified and strictly ordered flashblock payloads.
	/// Used by RPC overlays and other consumers of flashblock data.
	pub flashblock_tx: broadcast::Sender<FlashblocksPayloadV1>,
}

/// Handle for the flashblocks P2P protocol.
///
/// Encapsulates the shared context and mutable state of the flashblocks
/// P2P protocol.
#[derive(Clone, Debug)]
pub struct FlashblocksHandle {
	/// Shared context containing network handle, keys, and communication
	/// channels.
	pub ctx: FlashblocksP2PCtx,
	/// Thread-safe mutable state of the flashblocks protocol.
	/// Protected by a mutex to allow concurrent access from multiple
	/// connections.
	// TODO: right now I'm using world-chain FlashblocksP2pState. Later I'm gonna
	// build my own because there are useless fields in the world-chain one,
	// related to authorizations.
	pub state: Arc<Mutex<FlashblocksP2PState>>,
}

impl FlashblocksHandle {
	pub fn new(
		authorizer_vk: VerifyingKey,
		builder_sk: Option<SigningKey>,
	) -> Self {
		let flashblock_tx = broadcast::Sender::new(BROADCAST_BUFFER_CAPACITY);
		let peer_tx = broadcast::Sender::new(BROADCAST_BUFFER_CAPACITY);
		let state = Arc::new(Mutex::new(FlashblocksP2PState::default()));
		let ctx = FlashblocksP2PCtx {
			authorizer_vk,
			builder_sk,
			peer_tx,
			flashblock_tx,
		};

		Self { ctx, state }
	}

	pub fn flashblocks_tx(&self) -> broadcast::Sender<FlashblocksPayloadV1> {
		self.ctx.flashblock_tx.clone()
	}

	pub fn builder_sk(&self) -> Result<&SigningKey, FlashblocksP2PError> {
		self
			.ctx
			.builder_sk
			.as_ref()
			.ok_or(FlashblocksP2PError::MissingBuilderSk)
	}

	pub fn publish_new(&self, payload: FlashblocksPayloadV1) {
		let mut state = self.state.lock();
		self.ctx.publish(&mut state, payload);
	}
}

impl FlashblocksP2PCtx {
	pub fn publish(
		&self,
		state: &mut FlashblocksP2PState,
		payload: FlashblocksPayloadV1,
	) {
		let payload_index = payload.index;
		let payload_id = payload.payload_id;
		// Check if this is a globally new payload
		// TODO: fix the unwrap
		if payload.metadata.flashblock_timestamp.unwrap() as u64
			> state.payload_timestamp
		{
			state.payload_id = payload_id;
			state.payload_timestamp =
				payload.metadata.flashblock_timestamp.unwrap() as u64;
			state.flashblock_index = 0;
			state.flashblocks.fill(None);
		}

		// Resize our array if needed
		if payload_index as usize > MAX_FLASHBLOCK_INDEX {
			tracing::error!(
					target: "flashblocks::p2p",
					index = payload.index,
					max_index = MAX_FLASHBLOCK_INDEX,
					"Received flashblocks payload with index exceeding maximum"
			);
			return;
		}
		let len = state.flashblocks.len();
		state
			.flashblocks
			.resize_with(len.max(payload_index as usize + 1), || None);
		let flashblock = &mut state.flashblocks[payload_index as usize];

		// If we've already seen this index, skip it
		// Otherwise, add it to the list
		if flashblock.is_none() {
			// We haven't seen this index yet
			// Add the flashblock to our cache

			*flashblock = Some(payload.clone());
			tracing::trace!(
					target: "flashblocks::p2p",
					payload_id = %payload.payload_id,
					flashblock_index = payload_index,
					"queueing flashblock",
			);

			let p2p_msg = FlashblocksP2PMsg::FlashblocksPayloadV1(payload);
			let mut bytes = BytesMut::new();
			p2p_msg.encode(&mut bytes);
			let len = bytes.len();

			if len > MAX_FRAME {
				tracing::error!(
						target: "flashblocks::p2p",
						size = bytes.len(),
						max_size = MAX_FRAME,
						"FlashblocksP2PMsg too large",
				);
				return;
			}
			if len > MAX_FRAME / 2 {
				tracing::warn!(
						target: "flashblocks::p2p",
						size = bytes.len(),
						max_size = MAX_FRAME,
						"FlashblocksP2PMsg almost too large",
				);
			}

			let peer_msg = PeerMsg::FlashblocksPayloadV1((
				payload_id,
				payload_index as usize,
				bytes,
			));

			self.peer_tx.send(peer_msg).ok();

			let now = Utc::now()
				.timestamp_nanos_opt()
				.expect("time went backwards");

			// Broadcast any flashblocks in the cache that are in order
			while let Some(Some(flashblock_event)) =
				state.flashblocks.get(state.flashblock_index)
			{
				// Publish the flashblock
				self.flashblock_tx.send(flashblock_event.clone()).ok();

				// Don't measure the interval at the block boundary
				if state.flashblock_index != 0 {
					let interval = now - state.flashblock_timestamp;
				}

				// Update the index and timestamp
				state.flashblock_timestamp = now;
				state.flashblock_index += 1;
			}
		}
	}
}
