use {
	crate::flashblocks::{
		p2p::{
			FlashblocksP2PMsg,
			FlashblocksP2PProtocol,
			MAX_FLASHBLOCK_INDEX,
			PeerMsg,
		},
		primitives::FlashblocksPayloadV1,
	},
	alloy_rlp::Decodable,
	chrono::Utc,
	futures::StreamExt,
	rblib::{
		alloy::primitives::bytes::BytesMut,
		reth::{
			ethereum::network::{
				api::PeerId,
				eth_wire::multiplex::ProtocolConnection,
			},
			network::{Peers, types::ReputationChangeKind},
			payload::PayloadId,
		},
	},
	std::{
		pin::Pin,
		task::{Context, Poll, ready},
	},
	tokio_stream::{Stream, wrappers::BroadcastStream},
};

/// Represents a single P2P connection for the flashblocks protocol.
///
/// This struct manages the bidirectional communication with a single peer in
/// the flashblocks P2P network. It handles incoming messages from the peer,
/// validates and processes them, and also streams outgoing messages that need
/// to be broadcast.
///
/// The connection implements the `Stream` trait to provide outgoing message
/// bytes that should be sent to the connected peer over the underlying protocol
/// connection.
pub struct FlashblocksConnection<N> {
	/// The flashblocks protocol handler that manages the overall protocol state.
	protocol: FlashblocksP2PProtocol<N>,
	/// The underlying protocol connection for sending and receiving raw bytes.
	conn: ProtocolConnection,
	/// The unique identifier of the connected peer.
	peer_id: PeerId,
	/// Receiver for peer messages to be sent to all peers.
	/// We send bytes over this stream to avoid repeatedly having to serialize
	/// the payloads.
	peer_rx: BroadcastStream<PeerMsg>,
	/// Most recent payload ID received from this peer to track payload
	/// transitions.
	payload_id: PayloadId,
	/// A list of flashblock indices that we have already received from
	/// this peer for the current payload, used to detect duplicate messages.
	received: Vec<bool>,
}

impl<N> FlashblocksConnection<N> {
	/// Creates a new `FlashblocksConnection` instance.
	///
	/// # Arguments
	/// * `protocol` - The flashblocks protocol handler managing the connection.
	/// * `conn` - The underlying protocol connection for sending and receiving
	///   messages.
	/// * `peer_id` - The unique identifier of the connected peer.
	/// * `peer_rx` - Receiver for peer messages to be sent to all peers.
	pub fn new(
		protocol: FlashblocksP2PProtocol<N>,
		conn: ProtocolConnection,
		peer_id: PeerId,
		peer_rx: BroadcastStream<PeerMsg>,
	) -> Self {
		Self {
			protocol,
			conn,
			peer_id,
			peer_rx,
			payload_id: PayloadId::default(),
			received: Vec::new(),
		}
	}
}

impl<N> Drop for FlashblocksConnection<N> {
	fn drop(&mut self) {
		tracing::info!(
				target: "flashblocks::p2p",
				peer_id = %self.peer_id,
				"dropping flashblocks connection"
		);
	}
}

impl<N: Peers + Unpin> Stream for FlashblocksConnection<N> {
	type Item = BytesMut;

	fn poll_next(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
	) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();

		loop {
			// Check if there are any flashblocks ready to broadcast to our peers.
			if let Poll::Ready(Some(res)) = this.peer_rx.poll_next_unpin(cx) {
				match res {
					Ok(peer_msg) => {
						match peer_msg {
							PeerMsg::FlashblocksPayloadV1((
								payload_id,
								flashblock_index,
								bytes,
							)) => {
								// Check if this flashblock actually originated from this peer.
								if this.payload_id != payload_id
									|| this.received.get(flashblock_index) != Some(&true)
								{
									tracing::trace!(
											target: "flashblocks::p2p",
											peer_id = %this.peer_id,
											%payload_id,
											%flashblock_index,
											"Broadcasting `FlashblocksPayloadV1` message to peer"
									);

									return Poll::Ready(Some(bytes));
								}
							}
						}
					}
					Err(error) => {
						tracing::error!(
								target: "flashblocks::p2p",
								%error,
								"failed to receive flashblocks message from peer_rx"
						);
					}
				}
			}

			// Check if there are any messages from the peer.
			let Some(buf) = ready!(this.conn.poll_next_unpin(cx)) else {
				return Poll::Ready(None);
			};

			let msg = match FlashblocksP2PMsg::decode(&mut &buf[..]) {
				Ok(msg) => msg,
				Err(error) => {
					tracing::warn!(
							target: "flashblocks::p2p",
							peer_id = %this.peer_id,
							%error,
							"failed to decode flashblocks message from peer",
					);
					this
						.protocol
						.network
						.reputation_change(this.peer_id, ReputationChangeKind::BadMessage);
					return Poll::Ready(None);
				}
			};

			match msg {
				FlashblocksP2PMsg::FlashblocksPayloadV1(payload) => {
					this.handle_flashblocks_payload_v1(payload);
				}
			}
		}
	}
}

impl<N: Peers> FlashblocksConnection<N> {
	fn handle_flashblocks_payload_v1(&mut self, payload: FlashblocksPayloadV1) {
		let mut state = self.protocol.handle.state.lock();

		// Check if this is a new payload from this peer
		if self.payload_id != payload.payload_id {
			self.payload_id = payload.payload_id;
			self.received.fill(false);
		}

		// Check if the payload index is within the allowed range
		if payload.index as usize > MAX_FLASHBLOCK_INDEX {
			tracing::error!(
					target: "flashblocks::p2p",
					peer_id = %self.peer_id,
					index = payload.index,
					payload_id = %payload.payload_id,
					max_index = MAX_FLASHBLOCK_INDEX,
					"Received flashblocks payload with index exceeding maximum"
			);
			return;
		}

		// Check if this peer is spamming us with the same payload index
		let len = self.received.len();
		self
			.received
			.resize_with(len.max(payload.index as usize + 1), || false);
		if self.received[payload.index as usize] {
			// We've already seen this index from this peer.
			// They could be trying to DOS us.
			tracing::warn!(
					target: "flashblocks::p2p",
					peer_id = %self.peer_id,
					payload_id = %payload.payload_id,
					index = payload.index,
					"received duplicate flashblock from peer",
			);
			self.protocol.network.reputation_change(
				self.peer_id,
				ReputationChangeKind::AlreadySeenTransaction,
			);
			return;
		}
		self.received[payload.index as usize] = true;

		let now = Utc::now()
			.timestamp_nanos_opt()
			.expect("time went backwards");

		if let Some(flashblock_timestamp) = payload.metadata.flashblock_timestamp {
			let _latency = now - flashblock_timestamp;
		}

		self.protocol.handle.ctx.publish(&mut state, payload);
	}
}
