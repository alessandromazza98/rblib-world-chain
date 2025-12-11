use {
	alloy_rlp::{Decodable, Encodable, Header},
	flashblocks_primitives::primitives::FlashblocksPayloadV1,
	serde::{Deserialize, Serialize},
};

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
