use {
	crate::flashblocks::{
		primitives::FlashblocksPayloadV1,
		state::FlashblocksStateExecutor,
	},
	alloy_rlp::{BytesMut, Decodable, Encodable, Header},
	chrono::Utc,
	parking_lot::Mutex,
	rblib::reth::{optimism::node::OpBuiltPayload, payload::PayloadId},
	serde::{Deserialize, Serialize},
	std::sync::Arc,
	tokio::sync::broadcast,
};

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

/// A message that can be sent over the Flashblocks P2P network.
///
/// This enum represents the top-level message types that can be transmitted
/// over the P2P network.
#[repr(u8)]
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, Eq)]
pub enum FlashblocksP2PMsg {
	/// A flashblock payload containing a list of transactions and associated
	/// metadata
	FlashblocksPayloadV1(FlashblocksPayloadV1) = 0x00,
}

impl Encodable for FlashblocksP2PMsg {
	fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
		match self {
			Self::FlashblocksPayloadV1(payload) => {
				Header {
					list: true,
					payload_length: 1 + payload.length(),
				}
				.encode(out);
				0u32.encode(out);
				payload.encode(out);
			}
		};
	}

	fn length(&self) -> usize {
		let body_len = match self {
			Self::FlashblocksPayloadV1(payload) => 1 + payload.length(),
		};

		Header {
			list: true,
			payload_length: body_len,
		}
		.length()
			+ body_len
	}
}

impl Decodable for FlashblocksP2PMsg {
	fn decode(buf: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
		let hdr = Header::decode(buf)?;
		if !hdr.list {
			return Err(alloy_rlp::Error::Custom(
				"FlashblocksP2PMsg must be an RLP list",
			));
		}

		let tag = u8::decode(buf)?;
		let value = match tag {
			0 => Self::FlashblocksPayloadV1(FlashblocksPayloadV1::decode(buf)?),
			_ => return Err(alloy_rlp::Error::Custom("unknown tag")),
		};

		Ok(value)
	}
}

/// Messages that can be broadcast over a channel to each internal peer
/// connection.
///
/// These messages are used internally to coordinate the broadcasting of
/// flashblocks and publishing status changes to all connected peers.
#[derive(Clone, Debug)]
pub enum PeerMsg {
	/// Send an already serialized flashblock to all peers.
	FlashblocksPayloadV1((PayloadId, usize, BytesMut)),
}

/// Protocol state that stores the flashblocks P2P protocol events and
/// coordination data.
///
/// This struct maintains the current state of flashblock publishing, including
/// coordination with other publishers, payload buffering, and ordering
/// information. It serves as the central state management for the flashblocks
/// P2P protocol handler.
#[derive(Debug, Default)]
pub struct FlashblocksP2PState {
	/// Most recent payload ID for the current block being processed.
	pub payload_id: PayloadId,
	/// Timestamp of the most recent flashblocks payload.
	pub payload_timestamp: u64,
	/// Timestamp at which the most recent flashblock was received in ns since
	/// the unix epoch.
	pub flashblock_timestamp: i64,
	/// The index of the next flashblock to emit over the flashblocks stream.
	/// Used to maintain strict ordering of flashblock delivery.
	pub flashblock_index: usize,
	/// Buffer of flashblocks for the current payload, indexed by flashblock
	/// sequence number. Contains `None` for flashblocks not yet received,
	/// enabling out-of-order receipt while maintaining in-order delivery.
	pub flashblocks: Vec<Option<FlashblocksPayloadV1>>,
}

/// Context struct containing shared resources for the flashblocks P2P protocol.
///
/// This struct holds the network handle, cryptographic keys, and communication
/// channels used across all connections in the flashblocks P2P protocol. It
/// provides the shared infrastructure needed for message verification, signing,
/// and broadcasting.
#[derive(Clone, Debug)]
pub struct FlashblocksP2PCtx {
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
	pub fn new() -> Self {
		let flashblock_tx = broadcast::Sender::new(BROADCAST_BUFFER_CAPACITY);
		let peer_tx = broadcast::Sender::new(BROADCAST_BUFFER_CAPACITY);
		let state = Arc::new(Mutex::new(FlashblocksP2PState::default()));
		let ctx = FlashblocksP2PCtx {
			peer_tx,
			flashblock_tx,
		};

		Self { ctx, state }
	}

	pub fn flashblocks_tx(&self) -> broadcast::Sender<FlashblocksPayloadV1> {
		self.ctx.flashblock_tx.clone()
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
