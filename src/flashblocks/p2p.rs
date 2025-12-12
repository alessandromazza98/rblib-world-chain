use {
	crate::flashblocks::{
		connection::FlashblocksConnection,
		primitives::FlashblocksPayloadV1,
		state::FlashblocksStateExecutor,
	},
	alloy_rlp::{BytesMut, Decodable, Encodable, Header},
	chrono::Utc,
	core::{fmt, net::SocketAddr},
	parking_lot::Mutex,
	rblib::reth::{
		eth_wire_types::Capability,
		ethereum::network::{
			api::PeerId,
			eth_wire::{
				capability::SharedCapabilities,
				multiplex::ProtocolConnection,
				protocol::Protocol,
			},
		},
		network::{
			Direction,
			Peers,
			protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler},
		},
		optimism::node::OpBuiltPayload,
		payload::PayloadId,
	},
	serde::{Deserialize, Serialize},
	std::sync::Arc,
	tokio::sync::broadcast,
	tokio_stream::wrappers::BroadcastStream,
};

/// Maximum frame size for rlpx messages.
pub(crate) const MAX_FRAME: usize = 1 << 24; // 16 MiB

/// Maximum index for flashblocks payloads.
/// Not intended to ever be hit. Since we resize the flashblocks vector
/// dynamically, this is just a sanity check to prevent excessive memory usage.
pub(crate) const MAX_FLASHBLOCK_INDEX: usize = 100;

/// The maximum number of broadcast channel messages we will buffer
/// before dropping them. In practice, we should rarely need to buffer any
/// messages.
pub(crate) const BROADCAST_BUFFER_CAPACITY: usize = 100;

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
					let _interval = now - state.flashblock_timestamp;
				}

				// Update the index and timestamp
				state.flashblock_timestamp = now;
				state.flashblock_index += 1;
			}
		}
	}
}

/// Main protocol handler for the flashblocks P2P protocol.
///
/// This handler manages incoming and outgoing connections, coordinates
/// flashblock publishing, and maintains the protocol state across all peer
/// connections. It implements the core logic for multi-builder coordination and
/// failover scenarios in HA sequencer setups.
#[derive(Clone, Debug)]
pub struct FlashblocksP2PProtocol<N> {
	/// Network handle used to update peer reputation and manage connections.
	pub network: N,
	/// Shared context containing network handle, keys, and communication
	/// channels.
	pub handle: FlashblocksHandle,
}

impl<N> FlashblocksP2PProtocol<N> {
	/// Creates a new flashblocks P2P protocol handler.
	///
	/// Initializes the handler with the necessary cryptographic keys, network
	/// handle, and communication channels. The handler starts in a
	/// non-publishing state.
	///
	/// # Arguments
	/// * `network` - Network handle for peer management and reputation updates
	/// * `handle` - Shared handle containing the protocol context and mutable
	///   state
	pub fn new(network: N, handle: FlashblocksHandle) -> Self {
		Self { network, handle }
	}
}

impl<N> FlashblocksP2PProtocol<N> {
	/// Returns the P2P capability for the flashblocks v1 protocol.
	///
	/// This capability is used during devp2p handshake to advertise support
	/// for the flashblocks protocol with protocol name "flblk" and version 1.
	pub fn capability() -> Capability {
		Capability::new_static("flblk", 1)
	}
}

impl<N: Peers + Unpin + fmt::Debug + Clone + Send + Sync + 'static>
	ProtocolHandler for FlashblocksP2PProtocol<N>
{
	type ConnectionHandler = Self;

	fn on_incoming(
		&self,
		_socket_addr: SocketAddr,
	) -> Option<Self::ConnectionHandler> {
		Some(self.clone())
	}

	fn on_outgoing(
		&self,
		_socket_addr: SocketAddr,
		_peer_id: PeerId,
	) -> Option<Self::ConnectionHandler> {
		Some(self.clone())
	}
}

impl<N: Peers + Unpin + Send + Sync + 'static> ConnectionHandler
	for FlashblocksP2PProtocol<N>
{
	type Connection = FlashblocksConnection<N>;

	fn protocol(&self) -> Protocol {
		Protocol::new(Self::capability(), 1)
	}

	fn on_unsupported_by_peer(
		self,
		_supported: &SharedCapabilities,
		_direction: Direction,
		_peer_id: PeerId,
	) -> OnNotSupported {
		OnNotSupported::KeepAlive
	}

	fn into_connection(
		self,
		direction: Direction,
		peer_id: PeerId,
		conn: ProtocolConnection,
	) -> Self::Connection {
		let capability = Self::capability();

		tracing::info!(
				target: "flashblocks::p2p",
				%peer_id,
				%direction,
				capability = %capability.name,
				version = %capability.version,
				"new flashblocks connection"
		);

		let peer_rx = self.handle.ctx.peer_tx.subscribe();

		FlashblocksConnection::new(
			self,
			conn,
			peer_id,
			BroadcastStream::new(peer_rx),
		)
	}
}
