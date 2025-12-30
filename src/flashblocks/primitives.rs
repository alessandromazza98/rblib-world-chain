use {
	alloy_rlp::{
		Decodable,
		Encodable,
		Header as RlpHeader,
		RlpDecodable,
		RlpEncodable,
	},
	chrono::Utc,
	eyre::{bail, eyre},
	rblib::{
		alloy::{
			consensus::{
				Block,
				BlockHeader,
				EMPTY_OMMER_ROOT_HASH,
				proofs::ordered_trie_root_with_encoder,
			},
			eips::{Decodable2718, Encodable2718, eip4895, merge::BEACON_NONCE},
			optimism::consensus::OpTxEnvelope,
			primitives::{Address, B64, B256, Bloom, Bytes, FixedBytes, U256},
			serde::quantity,
		},
		reth::{
			api::{Block as BlockApi, BlockBody as BlockBodyApi, NodePrimitives},
			optimism::{
				node::{OpBuiltPayload, OpPayloadBuilderAttributes},
				primitives::OpPrimitives,
			},
			payload::{PayloadBuilderAttributes, PayloadId, builder::PayloadConfig},
			primitives::{BlockBody, Header, RecoveredBlock},
			rpc::types::Withdrawal,
		},
	},
	serde::{Deserialize, Serialize, de::DeserializeOwned},
};

/// Represents the modified portions of an execution payload within a
/// flashblock. This structure contains only the fields that can be updated
/// during block construction, such as state root, receipts, logs, and new
/// transactions. Other immutable block fields like parent hash and block number
/// are excluded since they remain constant throughout the block's construction.
#[derive(
	Clone,
	Debug,
	PartialEq,
	Default,
	Deserialize,
	Serialize,
	Eq,
	RlpEncodable,
	RlpDecodable,
)]
pub struct ExecutionPayloadFlashblockDeltaV1 {
	/// The state root of the block.
	pub state_root: B256,
	/// The receipts root of the block.
	pub receipts_root: B256,
	/// The logs bloom of the block.
	pub logs_bloom: Bloom,
	/// The gas used of the block.
	#[serde(with = "quantity")]
	pub gas_used: u64,
	/// The block hash of the block.
	pub block_hash: B256,
	/// The transactions of the block.
	pub transactions: Vec<Bytes>,
	/// Array of [`Withdrawal`] enabled with V2
	pub withdrawals: Vec<Withdrawal>,
	/// The withdrawals root of the block.
	pub withdrawals_root: B256,
}

/// Represents the base configuration of an execution payload that remains
/// constant throughout block construction. This includes fundamental block
/// properties like parent hash, block number, and other header fields that are
/// determined at block creation and cannot be modified.
#[derive(
	Clone,
	Debug,
	PartialEq,
	Default,
	Deserialize,
	Serialize,
	Eq,
	RlpEncodable,
	RlpDecodable,
)]
pub struct ExecutionPayloadBaseV1 {
	/// Ecotone parent beacon block root
	pub parent_beacon_block_root: B256,
	/// The parent hash of the block.
	pub parent_hash: B256,
	/// The fee recipient of the block.
	pub fee_recipient: Address,
	/// The previous randao of the block.
	pub prev_randao: B256,
	/// The block number.
	#[serde(with = "quantity")]
	pub block_number: u64,
	/// The gas limit of the block.
	#[serde(with = "quantity")]
	pub gas_limit: u64,
	/// The timestamp of the block.
	#[serde(with = "quantity")]
	pub timestamp: u64,
	/// The extra data of the block.
	pub extra_data: Bytes,
	/// The base fee per gas of the block.
	pub base_fee_per_gas: U256,
}

#[derive(Clone, Default, Debug, PartialEq, Deserialize, Serialize, Eq)]
pub struct FlashblockMetadata {
	/// Total fees collected by the proposer for this block.
	pub fees: U256,
	/// The timestamp of when the flashblock was created in ns since the unix
	/// epoch
	#[serde(skip_serializing_if = "Option::is_none")]
	pub flashblock_timestamp: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Default, Deserialize, Serialize, Eq)]
pub struct FlashblocksPayloadV1<M = FlashblockMetadata> {
	/// The payload id of the flashblock
	pub payload_id: PayloadId,
	/// The index of the flashblock in the block
	pub index: u64,
	/// The delta/diff containing modified portions of the execution payload
	pub diff: ExecutionPayloadFlashblockDeltaV1,
	/// Additional metadata associated with the flashblock
	pub metadata: M,
	/// The base execution payload configuration
	#[serde(skip_serializing_if = "Option::is_none")]
	pub base: Option<ExecutionPayloadBaseV1>,
}

/// RLP empty string length (single byte `0x80`).
const RLP_EMPTY_LEN: usize = 1;

/// Manual RLP implementation because `PayloadId` and `serde_json::Value` are
/// outside of alloy-rlp's blanket impls.
impl<M> Encodable for FlashblocksPayloadV1<M>
where
	M: Serialize,
{
	fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
		let (payload_len, json_bytes) = self.compute_payload_length();

		RlpHeader {
			list: true,
			payload_length: payload_len,
		}
		.encode(out);

		// Encode fields in order:
		// 1. payload_id – the inner B64 already impls Encodable
		self.payload_id.0.encode(out);
		// 2. index
		self.index.encode(out);
		// 3. diff
		self.diff.encode(out);
		// 4. metadata (as raw JSON bytes)
		json_bytes.encode(out);
		// 5. base (Option encoded as value or empty string)
		if let Some(base) = &self.base {
			base.encode(out);
		} else {
			out.put_u8(0x80);
		}
	}

	fn length(&self) -> usize {
		let (payload_len, _) = self.compute_payload_length();
		RlpHeader {
			list: true,
			payload_length: payload_len,
		}
		.length()
			+ payload_len
	}
}

impl<M: Serialize> FlashblocksPayloadV1<M> {
	/// Computes the RLP payload length and serialized metadata bytes.
	///
	/// Returns (payload_length, json_bytes) to avoid duplicate serialization.
	fn compute_payload_length(&self) -> (usize, Bytes) {
		let json_bytes = Bytes::from(
			serde_json::to_vec(&self.metadata)
				.expect("serializing `metadata` to JSON never fails"),
		);

		let base_len = self
			.base
			.as_ref()
			.map(|b| b.length())
			.unwrap_or(RLP_EMPTY_LEN);

		let payload_len = self.payload_id.0.length()
			+ self.index.length()
			+ self.diff.length()
			+ json_bytes.length()
			+ base_len;

		(payload_len, json_bytes)
	}
}

impl<M> Decodable for FlashblocksPayloadV1<M>
where
	M: DeserializeOwned,
{
	fn decode(buf: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
		let header = RlpHeader::decode(buf)?;
		if !header.list {
			return Err(alloy_rlp::Error::UnexpectedString);
		}

		// Limit the decoding window to the list payload only.
		let mut body = &buf[..header.payload_length];

		let payload_id = B64::decode(&mut body)?.into();
		let index = u64::decode(&mut body)?;
		let diff = ExecutionPayloadFlashblockDeltaV1::decode(&mut body)?;

		// metadata – stored as raw JSON bytes
		let meta_bytes = Bytes::decode(&mut body)?;
		let metadata = serde_json::from_slice(&meta_bytes)
			.map_err(|_| alloy_rlp::Error::Custom("bad JSON"))?;

		// base (`Option`)
		let base = if body.first() == Some(&0x80) {
			None
		} else {
			Some(ExecutionPayloadBaseV1::decode(&mut body)?)
		};

		// advance the original buffer cursor
		*buf = &buf[header.payload_length..];

		Ok(Self {
			payload_id,
			index,
			diff,
			metadata,
			base,
		})
	}
}

/// A type wrapper around a single flashblock payload.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, Eq)]
pub struct Flashblock {
	pub flashblock: FlashblocksPayloadV1,
}

impl Flashblock {
	pub fn new(
		payload: &OpBuiltPayload,
		config: PayloadConfig<OpPayloadBuilderAttributes<OpTxEnvelope>, Header>,
		index: u64,
		transactions_offset: usize,
		withdrawal_offset: usize,
	) -> Self {
		let block = payload.block();
		let fees = payload.fees();

		// todo cache trie updated
		let payload_base = if index == 0 {
			Some(ExecutionPayloadBaseV1 {
				parent_beacon_block_root: config
					.attributes
					.payload_attributes
					.parent_beacon_block_root
					.unwrap_or_default(),
				parent_hash: config.attributes.parent(),
				fee_recipient: config
					.attributes
					.payload_attributes
					.suggested_fee_recipient(),
				prev_randao: config.attributes.payload_attributes.prev_randao,
				block_number: block.number(),
				gas_limit: block.gas_limit(),
				timestamp: config.attributes.payload_attributes.timestamp,
				extra_data: block.extra_data().clone(),
				base_fee_per_gas: block
					.base_fee_per_gas()
					.map(U256::from)
					.unwrap_or_default(),
			})
		} else {
			None
		};

		let transactions = block
			.body()
			.transactions_iter()
			.skip(transactions_offset)
			.map(|tx| tx.encoded_2718().into())
			.collect::<Vec<_>>();

		let withdrawals = block
			.body()
			.withdrawals()
			.map(|withdrawals| {
				withdrawals
					.into_iter()
					.cloned()
					.skip(withdrawal_offset)
					.collect::<Vec<_>>()
			})
			.unwrap_or_default();

		let metadata = FlashblockMetadata {
			fees,
			flashblock_timestamp: Some(
				Utc::now()
					.timestamp_nanos_opt()
					.expect("time went backwards"),
			),
		};

		Flashblock {
			flashblock: FlashblocksPayloadV1 {
				payload_id: config.attributes.payload_id(),
				index,
				base: payload_base,
				diff: ExecutionPayloadFlashblockDeltaV1 {
					state_root: block.state_root(),
					receipts_root: block.receipts_root(),
					logs_bloom: block.logs_bloom(),
					gas_used: block.gas_used(),
					block_hash: block.hash(),
					transactions,
					withdrawals,
					withdrawals_root: block.withdrawals_root().unwrap_or_default(),
				},
				metadata,
			},
		}
	}
}

impl Flashblock {
	pub fn flashblock(&self) -> &FlashblocksPayloadV1 {
		&self.flashblock
	}

	pub fn into_flashblock(self) -> FlashblocksPayloadV1 {
		self.flashblock
	}

	pub fn payload_id(&self) -> &FixedBytes<8> {
		&self.flashblock.payload_id.0
	}

	pub fn base(&self) -> Option<&ExecutionPayloadBaseV1> {
		self.flashblock.base.as_ref()
	}

	pub fn diff(&self) -> &ExecutionPayloadFlashblockDeltaV1 {
		&self.flashblock.diff
	}
}

impl Flashblock {
	pub fn reduce(flashblocks: Flashblocks) -> Option<Flashblock> {
		let mut iter = flashblocks.0.into_iter();
		let mut acc = iter.next()?.flashblock;

		for next in iter {
			debug_assert_eq!(
				acc.payload_id, next.flashblock.payload_id,
				"all flashblocks should have the same payload_id"
			);

			if acc.base.is_none() && next.flashblock.base.is_some() {
				acc.base = next.flashblock.base;
			}

			acc.index = next.flashblock.index;

			acc.metadata.fees = next.flashblock.metadata.fees;
			acc.metadata.flashblock_timestamp =
				next.flashblock.metadata.flashblock_timestamp;

			acc.diff.gas_used = next.flashblock.diff.gas_used;

			acc
				.diff
				.transactions
				.extend(next.flashblock.diff.transactions);
			acc
				.diff
				.withdrawals
				.extend(next.flashblock.diff.withdrawals);

			acc.diff.state_root = next.flashblock.diff.state_root;
			acc.diff.receipts_root = next.flashblock.diff.receipts_root;
			acc.diff.logs_bloom = next.flashblock.diff.logs_bloom;
			acc.diff.withdrawals_root = next.flashblock.diff.withdrawals_root;
			acc.diff.block_hash = next.flashblock.diff.block_hash;
		}

		Some(Flashblock { flashblock: acc })
	}
}

impl TryFrom<Flashblock> for RecoveredBlock<Block<OpTxEnvelope>> {
	type Error = eyre::Report;

	/// Do _not_ use this method unless all flashblocks have been properly reduced
	fn try_from(
		value: Flashblock,
	) -> Result<RecoveredBlock<Block<OpTxEnvelope>>, Self::Error> {
		let base = value
			.base()
			.ok_or(eyre!("Flashblock is missing base payload"))?;
		let diff = value.flashblock.diff.clone();
		let header = Header {
			parent_beacon_block_root: None,
			state_root: diff.state_root,
			receipts_root: diff.receipts_root,
			logs_bloom: diff.logs_bloom,
			withdrawals_root: Some(diff.withdrawals_root),
			parent_hash: base.parent_hash,
			base_fee_per_gas: Some(base.base_fee_per_gas.to()),
			beneficiary: base.fee_recipient,
			transactions_root: ordered_trie_root_with_encoder(
				&diff.transactions,
				|tx, e| *e = tx.as_ref().to_vec(),
			),
			ommers_hash: EMPTY_OMMER_ROOT_HASH,
			blob_gas_used: Some(0),
			difficulty: U256::ZERO,
			number: base.block_number,
			gas_limit: base.gas_limit,
			gas_used: diff.gas_used,
			timestamp: base.timestamp,
			extra_data: base.extra_data.clone(),
			mix_hash: base.prev_randao,
			nonce: BEACON_NONCE.into(),
			requests_hash: None, // TODO: Isthmus
			excess_blob_gas: Some(0),
			block_access_list_hash: Some(B256::ZERO), // TODO: change it
		};

		let transactions_encoded = diff
			.transactions
			.iter()
			.map(|t| {
				<OpPrimitives as NodePrimitives>::SignedTx::decode_2718(&mut t.as_ref())
			})
			.collect::<Result<Vec<_>, _>>()
			.map_err(|e| eyre!("Failed to decode transaction: {:?}", e))?;

		let body = BlockBody {
			transactions: transactions_encoded,
			withdrawals: Some(eip4895::Withdrawals(diff.withdrawals.to_vec())),
			ommers: vec![],
			block_access_list: None, // TODO: change it
		};

		Block::new(header, body)
			.try_into_recovered()
			.map_err(|e| eyre!("Failed to recover block: {:?}", e))
	}
}

/// A collection of flashblocks mapped to the same payload ID.
///
/// Guaranteed to be
/// - Non-empty
/// - All flashblocks have the same payload ID
/// - Flashblocks have contiguous indices starting from 0
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flashblocks(Vec<Flashblock>);

impl Default for Flashblocks {
	fn default() -> Self {
		Flashblocks(vec![Flashblock {
			flashblock: FlashblocksPayloadV1 {
				base: Some(ExecutionPayloadBaseV1::default()),
				..Default::default()
			},
		}])
	}
}

impl Flashblocks {
	/// Creates a new `Flashblocks` collection from the given vector of
	/// flashblocks.
	///
	/// Validates that the collection is non-empty, all flashblocks have the same
	/// payload ID, and that the indices are contiguous starting from 0.
	/// Returns an error if any of these conditions are not met.
	pub fn new(flashblocks: Vec<Flashblock>) -> eyre::Result<Self> {
		if flashblocks.is_empty() {
			bail!("Flashblocks cannot be empty")
		}

		let mut iter = flashblocks.iter();

		let first = iter.next().unwrap();
		if first.flashblock.base.is_none() {
			bail!("The first flashblock must contain the base payload");
		}

		let payload_id = first.payload_id();
		if iter.any(|fb| fb.payload_id() != payload_id) {
			bail!("All flashblocks must have the same payload_id")
		}

		for (i, fb) in flashblocks.iter().enumerate() {
			if fb.flashblock.index != i as u64 {
				bail!("Flashblocks must have contiguous indices starting from 0");
			}
		}

		Ok(Self(flashblocks))
	}

	/// Pushes a new flashblock into the collection.
	///
	/// If the new flashblock has a different payload ID, the collection is
	/// cleared and the new flashblock is added as the first element (index must
	/// be 0). If the new flashblock has the same payload ID, it is added to the
	/// end of the collection (index must be contiguous).
	/// Returns `Ok(true)` if the collection was reset, `Ok(false)` if the
	/// flashblock was added to the existing collection, or an error if the
	/// conditions are not met.
	pub fn push(&mut self, flashblock: Flashblock) -> eyre::Result<bool> {
		let is_new_payload = self.is_new_payload(&flashblock)?;
		if is_new_payload {
			self.0.clear();
		}
		self.0.push(flashblock);
		Ok(is_new_payload)
	}

	pub fn is_new_payload(&self, flashblock: &Flashblock) -> eyre::Result<bool> {
		if flashblock.payload_id() != self.payload_id() {
			if flashblock.flashblock.index != 0 {
				bail!("New flashblock has different payload_id and index is not 0");
			}
			let Some(base) = &flashblock.flashblock.base else {
				bail!(
					"New flashblock has different payload_id and must contain the base \
					 payload"
				);
			};
			if base.timestamp <= self.base().timestamp {
				bail!(
					"New flashblock has different payload_id and must have a later \
					 timestamp than the current base"
				);
			}

			return Ok(true);
		}

		if flashblock.flashblock.index != (self.0.len() as u64) {
			bail!(
				"New flashblock index is not contiguous expected {}, got {}, \
				 payload_id: {}",
				self.0.len(),
				flashblock.flashblock.index,
				self.payload_id()
			);
		}

		Ok(false)
	}

	pub fn last(&self) -> &Flashblock {
		self.0.last().unwrap()
	}

	pub fn flashblocks(&self) -> &[Flashblock] {
		&self.0
	}

	pub fn payload_id(&self) -> &FixedBytes<8> {
		self.0.first().unwrap().payload_id()
	}

	pub fn base(&self) -> &ExecutionPayloadBaseV1 {
		self.0.first().unwrap().flashblock.base.as_ref().unwrap()
	}
}

impl TryFrom<Flashblocks> for RecoveredBlock<Block<OpTxEnvelope>> {
	type Error = eyre::Report;

	/// Converts a collection of flashblocks into a single `RecoveredBlock`.
	fn try_from(
		value: Flashblocks,
	) -> Result<RecoveredBlock<Block<OpTxEnvelope>>, Self::Error> {
		let reduced =
			Flashblock::reduce(value).ok_or(eyre!("No flashblocks to reduce"))?;
		reduced.try_into()
	}
}

#[cfg(test)]
mod tests {
	use {
		super::*,
		alloy_rlp::{Decodable, encode},
	};

	fn sample_diff() -> ExecutionPayloadFlashblockDeltaV1 {
		ExecutionPayloadFlashblockDeltaV1 {
			state_root: B256::from([1u8; 32]),
			receipts_root: B256::from([2u8; 32]),
			logs_bloom: Bloom::default(),
			gas_used: 21_000,
			block_hash: B256::from([3u8; 32]),
			transactions: vec![Bytes::from(vec![0xde, 0xad, 0xbe, 0xef])],
			withdrawals: vec![Withdrawal::default()],
			withdrawals_root: B256::from([4u8; 32]),
		}
	}

	fn sample_base() -> ExecutionPayloadBaseV1 {
		ExecutionPayloadBaseV1 {
			parent_beacon_block_root: B256::from([5u8; 32]),
			parent_hash: B256::from([6u8; 32]),
			fee_recipient: Address::from([0u8; 20]),
			prev_randao: B256::from([7u8; 32]),
			block_number: 123,
			gas_limit: 30_000_000,
			timestamp: 1_700_000_000,
			extra_data: Bytes::from(b"hello".to_vec()),
			base_fee_per_gas: U256::from(1_000_000_000u64),
		}
	}

	#[test]
	fn roundtrip_without_base() {
		let original = FlashblocksPayloadV1 {
			payload_id: PayloadId::default(),
			index: 0,
			diff: sample_diff(),
			metadata: serde_json::json!({ "key": "value" }),
			base: None,
		};

		let encoded = encode(&original);
		assert_eq!(
			encoded.len(),
			original.length(),
			"length() must match actually-encoded size"
		);

		let mut slice = encoded.as_ref();
		let decoded =
			FlashblocksPayloadV1::decode(&mut slice).expect("decode succeeds");
		assert_eq!(original, decoded, "round-trip must be loss-less");
		assert!(
			slice.is_empty(),
			"decoder should consume the entire input buffer"
		);
	}

	#[test]
	fn roundtrip_with_base() {
		let original = FlashblocksPayloadV1 {
			payload_id: PayloadId::default(),
			index: 42,
			diff: sample_diff(),
			metadata: serde_json::json!({ "foo": 1, "bar": [1, 2, 3] }),
			base: Some(sample_base()),
		};

		let encoded = encode(&original);
		assert_eq!(encoded.len(), original.length());

		let mut slice = encoded.as_ref();
		let decoded =
			FlashblocksPayloadV1::decode(&mut slice).expect("decode succeeds");
		assert_eq!(original, decoded);
		assert!(slice.is_empty());
	}

	#[test]
	fn invalid_rlp_is_rejected() {
		let valid = FlashblocksPayloadV1 {
			payload_id: PayloadId::default(),
			index: 1,
			diff: sample_diff(),
			metadata: serde_json::json!({}),
			base: None,
		};

		// Encode, then truncate the last byte to corrupt the payload.
		let mut corrupted = encode(&valid);
		corrupted.pop();

		let mut slice = corrupted.as_ref();
		let result = FlashblocksPayloadV1::<String>::decode(&mut slice);
		assert!(
			result.is_err(),
			"decoder must flag malformed / truncated input"
		);
	}
}
