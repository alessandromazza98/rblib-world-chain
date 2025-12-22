use {
	crate::{
		WorldChain,
		context::WorldContext,
		flashblocks::{
			p2p::FlashblocksP2p,
			primitives::{
				ExecutionPayloadBaseV1,
				ExecutionPayloadFlashblockDeltaV1,
				Flashblock,
				FlashblockMetadata,
				FlashblocksPayloadV1,
			},
			state::FlashblocksStateExecutor,
		},
	},
	atomic_time::AtomicOptionInstant,
	chrono::Utc,
	core::sync::atomic::{AtomicU64, Ordering},
	parking_lot::RwLock,
	rblib::{
		alloy::{
			consensus::{
				Block,
				BlockBody,
				BlockHeader,
				EMPTY_OMMER_ROOT_HASH,
				Eip658Value,
				Receipt,
				Transaction,
				TxReceipt,
				constants::EMPTY_WITHDRAWALS,
				proofs,
			},
			eips::{
				Encodable2718,
				eip7685::EMPTY_REQUESTS_HASH,
				merge::BEACON_NONCE,
			},
			optimism::consensus::{OpDepositReceipt, OpTxEnvelope},
			primitives::U256,
		},
		prelude::{
			BlockExt,
			Checkpoint,
			CheckpointExt,
			ControlFlow,
			Counter,
			Histogram,
			InitContext,
			MetricsSet,
			PayloadBuilderError,
			Span,
			SpanExt,
			Step,
			StepContext,
			ext::CheckpointOpExt,
			types,
		},
		reth::{
			errors::BlockExecutionError,
			evm::op_revm::OpHaltReason,
			optimism::{
				forks::OpHardforks,
				node::OpBuiltPayload,
				primitives::{OpPrimitives, OpReceipt, OpTxType},
			},
			payload::{BuiltPayload, PayloadBuilderAttributes},
			primitives::{Header, Recovered, RecoveredBlock, logs_bloom},
			provider::ExecutionOutcome,
			revm::{DatabaseRef, context::result::ExecutionResult},
			rpc::types::Withdrawals,
		},
		revm::database::{
			BundleState,
			states::reverts::{AccountInfoRevert, Reverts},
		},
	},
	reth_chain_state::ExecutedBlock,
	reth_optimism_consensus::{calculate_receipt_root_no_memo_optimism, isthmus},
	std::{
		collections::{HashMap, hash_map::Entry},
		sync::Arc,
		time::Instant,
	},
};

/// Flashblocks pipeline step for publishing flashblocks to external
/// subscribers.
///
/// This step will send a JSON serialized version of `FlashblocksPayloadV1` to
/// the websocket sink that spans all payload checkpoints since the last
/// barrier.
///
/// After publishing a flashblock it will place a new barrier in the payload
/// marking all checkpoints so far as immutable.
pub struct PublishFlashblock {
	/// p2p communication layer for flashblocks.
	p2p: FlashblocksP2p,

	/// Keeps track of the current flashblock number within the payload job.
	block_number: AtomicU64,

	/// Set once at the beginning of the payload job, captures immutable
	/// information about the payload that is being built. This info is derived
	/// from the payload attributes parameter on the FCU from the EL node.
	block_base: RwLock<Option<ExecutionPayloadBaseV1>>,

	/// Metrics for monitoring flashblock publishing.
	metrics: Metrics,

	/// Timestamps for various stages of the flashblock publishing process. This
	/// information is used to produce some of the metrics.
	times: Times,
}

impl PublishFlashblock {
	pub fn new(flashblocks_state: FlashblocksStateExecutor) -> Self {
		Self {
			p2p: FlashblocksP2p::new(flashblocks_state),
			block_number: AtomicU64::default(),
			block_base: RwLock::new(None),
			metrics: Metrics::default(),
			times: Times::default(),
		}
	}
}

impl Step<WorldChain> for PublishFlashblock {
	async fn step(
		self: std::sync::Arc<Self>,
		payload: Checkpoint<WorldChain>,
		ctx: StepContext<WorldChain>,
	) -> ControlFlow<WorldChain> {
		let op_built_payload = match self.build_op_built_payload(&payload, &ctx) {
			Ok(payload) => payload,
			Err(err) => return ControlFlow::Fail(err.into()),
		};

		let this_block_span = self.unpublished_payload(&payload);
		let transactions: Vec<_> = this_block_span
			.transactions()
			.map(|tx| tx.encoded_2718().into())
			.collect();

		// TODO: do we want to skip empty flashblocks? Or do we want to stream them
		// nevertheless?
		// if transactions.is_empty() {
		// 	// nothing to publish, empty flashblocks are not interesting, skip.
		// 	return ControlFlow::Ok(payload);
		// }

		// increment flashblock number
		let index = self.block_number.fetch_add(1, Ordering::SeqCst);

		let base = self.block_base.read().clone();
		let block = op_built_payload.block();
		let diff = ExecutionPayloadFlashblockDeltaV1 {
			state_root: block.state_root(),
			receipts_root: block.receipts_root(),
			logs_bloom: block.logs_bloom(),
			gas_used: block.gas_used(),
			block_hash: block.hash(),
			transactions,
			withdrawals: vec![],
			withdrawals_root: block.withdrawals_root().unwrap_or_default(),
		};
		let fees = op_built_payload.fees();
		let now = Utc::now()
			.timestamp_nanos_opt()
			.expect("time went backwards");
		let metadata = FlashblockMetadata {
			fees,
			flashblock_timestamp: Some(now),
		};
		let flashblock = Flashblock {
			flashblock: FlashblocksPayloadV1 {
				payload_id: ctx.block().payload_id(),
				index,
				diff,
				metadata,
				base,
			},
		};

		// Push the contents of the payload
		if let Err(e) = self
			.p2p
			.publish(flashblock.into_flashblock(), op_built_payload.clone())
		{
			tracing::error!("Failed to publish flashblock to p2p: {e}");

			// on transport error, do not place a barrier, just return the payload
			// as is. It may be picked up by the next iteration.
			// TODO: do we want to return error in this scenario?
			return ControlFlow::Ok(payload);
		}
		tracing::info!("🔥 flashblock published over p2p 🔥");

		// block published to WS successfully
		self.times.on_published_block(&self.metrics);
		self.capture_payload_metrics(&this_block_span);

		// Place a barrier after each published flashblock to freeze the contents
		// of the payload up to this point, since this becomes a publicly committed
		// state.
		let world_ctx = WorldContext::new(op_built_payload);
		let payload = payload.barrier_with_context(world_ctx);
		ControlFlow::Ok(payload)
	}

	/// Before the payload job starts prepare the contents of the
	/// `ExecutionPayloadBaseV1` since at this point we have all the information
	/// we need to construct it and its content do not change throughout the job.
	async fn before_job(
		self: Arc<Self>,
		ctx: StepContext<WorldChain>,
	) -> Result<(), PayloadBuilderError> {
		self.times.on_job_started(&self.metrics);

		// this remains constant for the entire payload job.
		self.block_base.write().replace(ExecutionPayloadBaseV1 {
			parent_beacon_block_root: ctx
				.block()
				.attributes()
				.parent_beacon_block_root()
				.unwrap_or_default(),
			parent_hash: ctx.block().parent().hash(),
			fee_recipient: ctx.block().coinbase(),
			prev_randao: ctx.block().attributes().prev_randao(),
			block_number: ctx.block().number(),
			gas_limit: ctx
				.block()
				.attributes()
				.gas_limit
				.unwrap_or_else(|| ctx.block().parent().header().gas_limit()),
			timestamp: ctx.block().timestamp(),
			extra_data: ctx.block().block_env().extra_data.clone(),
			base_fee_per_gas: U256::from(ctx.block().base_fee()),
		});

		Ok(())
	}

	/// After a payload job completes, capture metrics and reset payload job
	/// specific state.
	async fn after_job(
		self: Arc<Self>,
		_: StepContext<WorldChain>,
		_: Arc<Result<types::BuiltPayload<WorldChain>, PayloadBuilderError>>,
	) -> Result<(), PayloadBuilderError> {
		self.times.on_job_ended(&self.metrics);

		// reset flashblocks block counter
		let count = self.block_number.swap(0, Ordering::SeqCst);
		self.metrics.blocks_per_payload_job.record(count as f64);
		*self.block_base.write() = None;

		Ok(())
	}

	/// Called during pipeline instantiation before any payload job is served.
	/// - Configure metrics scope.
	async fn setup(
		&mut self,
		ctx: InitContext<WorldChain>,
	) -> Result<(), PayloadBuilderError> {
		self.metrics = Metrics::with_scope(ctx.metrics_scope());
		Ok(())
	}
}

impl PublishFlashblock {
	/// Creates a op built payload from the provided checkpoint. If there is an
	/// already built payload in a previous checkpoint barrier, then use it as a
	/// pre-state for this new payload that will be built.
	fn build_op_built_payload(
		&self,
		payload: &Checkpoint<WorldChain>,
		ctx: &StepContext<WorldChain>,
	) -> Result<OpBuiltPayload, BlockExecutionError> {
		let chain_spec = payload.block().chainspec();
		let timestamp = payload.block().timestamp();

		// Initialize state from previous barrier's built payload if available,
		// otherwise start fresh with base bundle state.
		let (
			mut bundle_state,
			mut receipts,
			mut total_fees,
			mut cumulative_gas_used,
		) = self.extract_previous_state(payload)?;

		let base_fee = payload.block().base_fee();

		let this_block_span = self.unpublished_payload(payload);

		// Process each checkpoint in the unpublished span, accumulating state
		// changes, receipts, and fee calculations.
		for checkpoint in this_block_span {
			let Some(checkpoint_bundle_state) = checkpoint.state().cloned() else {
				continue;
			};
			bundle_state.extend(checkpoint_bundle_state);

			let checkpoint_txs = checkpoint.transactions();
			for (i, tx) in checkpoint_txs.iter().enumerate() {
				let Some(result) =
					checkpoint.result().map(|res| res.results()[i].clone())
				else {
					continue;
				};

				let gas_used = result.gas_used();
				cumulative_gas_used += gas_used;

				// Only accumulate miner fees for non-deposit transactions
				if !tx.is_deposit() {
					let miner_fee = tx
						.effective_tip_per_gas(base_fee)
						.expect("fee is always valid; execution succeeded");
					total_fees += U256::from(miner_fee) * U256::from(gas_used);
				}

				let receipt = self.build_receipt(
					tx,
					result,
					cumulative_gas_used,
					&checkpoint,
					ctx,
					chain_spec.as_ref(),
					timestamp,
				)?;
				receipts.push(receipt);
			}
		}

		let chain_spec = payload.block().chainspec();
		let txs: Vec<Recovered<OpTxEnvelope>> =
			payload.history().transactions().cloned().collect();
		let transactions_root = proofs::calculate_transaction_root(&txs);
		tracing::info!("receipts: {:?}", receipts);
		tracing::info!("timestamp: {:?}", timestamp);
		let receipts_root =
			calculate_receipt_root_no_memo_optimism(&receipts, chain_spec, timestamp);
		let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));
		// Flatten reverts into a single transition:
		// - per account: keep earliest `previous_status`
		// - per account: keep earliest non-`DoNothing` account-info revert
		// - per account+slot: keep earliest revert-to value
		// - per account: OR `wipe_storage`
		//
		// This keeps `bundle_state.reverts.len() == 1`, which matches the
		// expectation that this bundle represents a single block worth of changes
		// even if we built multiple payloads.
		let flattened = flatten_reverts(&bundle_state.reverts);
		bundle_state.reverts = flattened;
		let mut requests_hash = None;
		let withdrawals_root =
			if chain_spec.is_isthmus_active_at_timestamp(timestamp) {
				// always empty requests hash post isthmus
				requests_hash = Some(EMPTY_REQUESTS_HASH);
				// withdrawals root field in block header is used for storage root of L2
				// predeploy `l2tol1-message-passer`
				Some(
					isthmus::withdrawals_root(&bundle_state, ctx.provider())
						.map_err(BlockExecutionError::other)?,
				)
			} else if chain_spec.is_canyon_active_at_timestamp(timestamp) {
				Some(EMPTY_WITHDRAWALS)
			} else {
				None
			};
		let (excess_blob_gas, blob_gas_used) = if chain_spec
			.is_jovian_active_at_timestamp(timestamp)
		{
			let blob_gas_used = payload.cumulative_da_footprint().unwrap_or_default();
			// In jovian, we're using the blob gas used field to store the current
			// da footprint's value.
			(Some(0), Some(blob_gas_used))
		} else if chain_spec.is_ecotone_active_at_timestamp(timestamp) {
			(Some(0), Some(0))
		} else {
			(None, None)
		};
		let hashed_state = ctx.provider().hashed_post_state(&bundle_state);
		let (state_root, trie_updates) = ctx
			.provider()
			.state_root_with_updates(hashed_state.clone())
			.map_err(BlockExecutionError::other)?;

		let header = Header {
			parent_hash: payload.block().parent().hash(),
			ommers_hash: EMPTY_OMMER_ROOT_HASH,
			beneficiary: payload.block().evm_env().block_env.beneficiary,
			state_root,
			transactions_root,
			receipts_root,
			withdrawals_root,
			logs_bloom,
			timestamp,
			mix_hash: payload
				.block()
				.evm_env()
				.block_env
				.prevrandao
				.unwrap_or_default(),
			nonce: BEACON_NONCE.into(),
			base_fee_per_gas: Some(base_fee),
			number: payload.block().evm_env().block_env.number.saturating_to(),
			gas_limit: payload.block().evm_env().block_env.gas_limit,
			difficulty: payload.block().evm_env().block_env.difficulty,
			gas_used: cumulative_gas_used,
			extra_data: payload.block().block_env().extra_data.clone(),
			parent_beacon_block_root: payload
				.block()
				.block_env()
				.parent_beacon_block_root,
			blob_gas_used,
			excess_blob_gas,
			requests_hash,
		};

		let (transactions, senders) =
			txs.iter().map(|tx| tx.clone().into_parts()).unzip();
		let block = Block::new(header, BlockBody {
			transactions,
			ommers: Default::default(),
			withdrawals: Some(Withdrawals::default()), // empty withdrawals
		});
		let block = RecoveredBlock::new_unhashed(block, senders);
		let sealed_block = Arc::new(block.sealed_block().clone());
		let execution_outcome = ExecutionOutcome::new(
			bundle_state,
			vec![receipts],
			payload.block().number(),
			Vec::new(),
		);

		let executed: ExecutedBlock<OpPrimitives> = ExecutedBlock {
			recovered_block: Arc::new(block),
			execution_output: Arc::new(execution_outcome),
			hashed_state: Arc::new(hashed_state),
			trie_updates: Arc::new(trie_updates),
		};

		Ok(OpBuiltPayload::new(
			payload.block().payload_id(),
			sealed_block,
			U256::from(total_fees),
			Some(executed),
		))
	}

	/// Builds an OpReceipt from a transaction execution result.
	///
	/// Handles the different receipt types based on transaction type (deposit vs
	/// regular). For deposit transactions, includes deposit-specific fields like
	/// nonce and version.
	fn build_receipt(
		&self,
		tx: &Recovered<OpTxEnvelope>,
		result: ExecutionResult<OpHaltReason>,
		cumulative_gas_used: u64,
		checkpoint: &Checkpoint<WorldChain>,
		ctx: &StepContext<WorldChain>,
		chain_spec: &impl OpHardforks,
		timestamp: u64,
	) -> Result<OpReceipt, BlockExecutionError> {
		let receipt = Receipt {
			status: Eip658Value::Eip658(result.is_success()),
			cumulative_gas_used,
			logs: result.into_logs(),
		};

		match tx.tx_type() {
			OpTxType::Deposit => {
				// For deposits, we need to look up the sender's nonce from state
				let deposit_nonce = match checkpoint.prev() {
					Some(prev_checkpoint) => prev_checkpoint
						.basic_ref(tx.signer())
						.map_err(BlockExecutionError::other)?
						.map(|account| account.nonce),
					None => ctx
						.provider()
						.account_nonce(tx.signer_ref())
						.map_err(BlockExecutionError::other)?,
				};

				// The deposit receipt version was introduced in Canyon to indicate
				// an update to how receipt hashes should be computed.
				let deposit_receipt_version = chain_spec
					.is_canyon_active_at_timestamp(timestamp)
					.then_some(1);

				Ok(OpReceipt::Deposit(OpDepositReceipt {
					inner: receipt,
					deposit_nonce,
					deposit_receipt_version,
				}))
			}
			OpTxType::Legacy => Ok(OpReceipt::Legacy(receipt)),
			OpTxType::Eip2930 => Ok(OpReceipt::Eip2930(receipt)),
			OpTxType::Eip1559 => Ok(OpReceipt::Eip1559(receipt)),
			OpTxType::Eip7702 => Ok(OpReceipt::Eip7702(receipt)),
		}
	}

	/// Extracts the previous execution state from the latest barrier checkpoint.
	///
	/// Returns a tuple of (bundle_state, receipts, total_fees,
	/// cumulative_gas_used). If no previous built payload exists, returns fresh
	/// initial state.
	fn extract_previous_state(
		&self,
		payload: &Checkpoint<WorldChain>,
	) -> Result<(BundleState, Vec<OpReceipt>, U256, u64), BlockExecutionError> {
		// Check if we have a previous barrier with a built payload
		let Some(barrier) = payload.latest_barrier() else {
			// First flashblock for this payload id - start with base bundle state
			return Ok((payload.block().base_bundle_state(), vec![], U256::ZERO, 0));
		};

		let Some(op_built_payload) = &barrier.context().maybe_built_payload else {
			// Barrier exists but no built payload - start fresh
			return Ok((payload.block().base_bundle_state(), vec![], U256::ZERO, 0));
		};

		// Extract state from the previous built payload
		let executed_block = op_built_payload
			.executed_block()
			.expect("built payload must have execution outcome");

		let bundle_state = executed_block.execution_output.bundle.clone();

		// We always build op built payloads with exactly one block
		debug_assert_eq!(executed_block.execution_outcome().receipts.len(), 1);
		let receipts = executed_block
			.execution_output
			.receipts
			.first()
			.expect("receipts must have at least one block")
			.clone();

		let total_fees = op_built_payload.fees();
		let cumulative_gas_used = op_built_payload.block().gas_used();

		Ok((bundle_state, receipts, total_fees, cumulative_gas_used))
	}

	/// Returns a span that covers all payload checkpoints since the last barrier.
	/// Those are the transactions that are going to be published in this
	/// flashblock.
	///
	/// One exception is the first flashblock, we want to get all checkpoints
	/// since the begining of the block, because the `OptimismPrologue` step
	/// places a barrier after sequencer transactions and we want to broadcast
	/// those transactions as well.
	fn unpublished_payload(
		&self,
		payload: &Checkpoint<WorldChain>,
	) -> Span<WorldChain> {
		if self.block_number.load(Ordering::SeqCst) == 0 {
			// first block, get all checkpoints, including sequencer txs
			payload.history()
		} else {
			// subsequent block, get all checkpoints since last barrier
			payload.history_staging()
		}
	}

	/// Called for each flashblock to capture metrics about the produced
	/// flashblock contents.
	fn capture_payload_metrics(&self, span: &Span<WorldChain>) {
		self.metrics.blocks_total.increment(1);
		self.metrics.gas_per_block.record(span.gas_used() as f64);

		self
			.metrics
			.blob_gas_per_block
			.record(span.blob_gas_used() as f64);

		self
			.metrics
			.txs_per_block
			.record(span.transactions().count() as f64);

		let bundles_count = span.iter().filter(|c| c.is_bundle()).count();
		self.metrics.bundles_per_block.record(bundles_count as f64);
	}
}

#[derive(MetricsSet)]
struct Metrics {
	/// Total number of flashblocks published across all payloads.
	pub blocks_total: Counter,

	/// Histogram of gas usage per flashblock.
	pub gas_per_block: Histogram,

	/// Histogram of blob gas usage per flashblock.
	pub blob_gas_per_block: Histogram,

	/// Histogram of transactions per flashblock.
	pub txs_per_block: Histogram,

	/// Histogram of the number of bundles per flashblock.
	pub bundles_per_block: Histogram,

	/// Histogram of flashblocks per job.
	pub blocks_per_payload_job: Histogram,

	/// The time interval flashblocks within one block.
	pub intra_block_interval: Histogram,

	/// The time interval between flashblocks from two consecutive payload jobs.
	/// This measures time between last flashblock from block N and first
	/// flashblock from block N+1.
	pub inter_block_interval: Histogram,

	/// The time interval between the end of one payload job and the start of the
	/// next.
	pub inter_jobs_interval: Histogram,

	/// The time it takes between the beginning of a payload job
	/// until the first flashblock is published.
	pub time_to_first_block: Histogram,

	/// The time between the last published flashblock and the end of the payload
	/// job.
	pub idle_tail_time: Histogram,
}

/// Used to track timing information for metrics.
#[derive(Default)]
struct Times {
	pub job_started: AtomicOptionInstant,
	pub job_ended: AtomicOptionInstant,
	pub first_block_at: AtomicOptionInstant,
	pub previous_block_at: AtomicOptionInstant,
	pub last_block_at: AtomicOptionInstant,
}

impl Times {
	pub fn on_job_started(&self, metrics: &Metrics) {
		let now = Instant::now();
		self.job_started.store(Some(now), Ordering::Relaxed);

		if let Some(ended_at) = self.job_ended.swap(None, Ordering::Relaxed) {
			let duration = now.duration_since(ended_at);
			metrics.inter_jobs_interval.record(duration);
		}
	}

	pub fn on_job_ended(&self, metrics: &Metrics) {
		let now = Instant::now();

		if let Some(last_block_at) = self.last_block_at.load(Ordering::Relaxed) {
			let idle_tail_time = now.duration_since(last_block_at);
			metrics.idle_tail_time.record(idle_tail_time);
		}

		self.job_ended.store(Some(now), Ordering::Relaxed);
		self.job_started.store(None, Ordering::Relaxed);
		self.first_block_at.store(None, Ordering::Relaxed);
		self.previous_block_at.store(None, Ordering::Relaxed);
	}

	pub fn on_published_block(&self, metrics: &Metrics) {
		let now = Instant::now();
		let last_block_at = self.last_block_at.load(Ordering::Relaxed);

		let is_first_block = self
			.first_block_at
			.compare_exchange(None, Some(now), Ordering::Relaxed, Ordering::Relaxed)
			.is_ok();

		if is_first_block {
			// this is the first block, capture inter-block interval
			if let Some(last_block_at) = last_block_at {
				let duration = now.duration_since(last_block_at);
				metrics.inter_block_interval.record(duration);
			}

			// capture time to first block
			let job_started = self.job_started.load(Ordering::Relaxed);
			if let Some(job_started) = job_started {
				let duration = now.duration_since(job_started);
				metrics.time_to_first_block.record(duration);
			}
		}

		// store now as the last block time
		let prev_at = self.last_block_at.swap(Some(now), Ordering::Relaxed);
		self.previous_block_at.store(prev_at, Ordering::Relaxed);

		// capture the duration between consecutive flashblocks from the same
		// payload job.
		if !is_first_block && let Some(prev_at) = prev_at {
			let duration = now.duration_since(prev_at);
			metrics.intra_block_interval.record(duration);
		}
	}
}

/// Flattens a multi-transition [`Reverts`] into a single transition, merging
/// per-account data.
///
/// Merge rules (iterate earliest -> latest):
/// - For each account, keep the **earliest** `previous_status`.
/// - For each account, keep the **earliest non-`DoNothing`** account-info
///   revert.
/// - For each account+slot, keep the **earliest** `RevertToSlot`.
/// - For each account, OR `wipe_storage`.
fn flatten_reverts(reverts: &Reverts) -> Reverts {
	let mut per_account = HashMap::new();

	for (addr, acc_revert) in reverts.iter().flatten() {
		match per_account.entry(*addr) {
			Entry::Vacant(v) => {
				v.insert(acc_revert.clone());
			}
			Entry::Occupied(mut o) => {
				let entry = o.get_mut();

				// Always OR wipe_storage (if any transition wiped storage, the
				// block-level revert must reflect it).
				entry.wipe_storage |= acc_revert.wipe_storage;

				// Merge storage: keep earliest revert-to value per slot.
				for (slot, revert_to) in &acc_revert.storage {
					entry.storage.entry(*slot).or_insert(*revert_to);
				}

				// Merge account-info revert: keep earliest non-DoNothing.
				if matches!(entry.account, AccountInfoRevert::DoNothing)
					&& !matches!(acc_revert.account, AccountInfoRevert::DoNothing)
				{
					entry.account = acc_revert.account.clone();
				}

				// Keep earliest previous_status: do not overwrite.
			}
		}
	}

	// Transform the map into a vec
	let flattened = per_account.into_iter().collect();
	Reverts::new(vec![flattened])
}
