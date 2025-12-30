use {
	crate::flashblocks::{
		block_builder::FlashblocksBlockBuilder,
		block_executor::FlashblocksBlockExecutor,
		payload_txns::BestPayloadTxns,
	},
	alloy_rlp::Encodable,
	eyre::eyre,
	rblib::{
		alloy::{
			consensus::{Block, BlockHeader, Transaction},
			optimism::consensus::OpTxEnvelope,
			primitives::U256,
			signers::Either,
		},
		prelude::PayloadBuilderError,
		reth::{
			api::{BlockBody, BuiltPayloadExecutedBlock, NodePrimitives},
			core::primitives::SignedTransaction,
			errors::ProviderError,
			evm::{
				ConfigureEvm,
				Database,
				Evm,
				execute::{BlockBuilder, BlockBuilderOutcome},
				precompiles::PrecompilesMap,
			},
			optimism::{
				chainspec::{OpChainSpec, OpHardforks},
				evm::{
					OpEvm,
					OpEvmConfig,
					OpNextBlockEnvAttributes,
					OpRethReceiptBuilder,
				},
				node::{
					OpBuiltPayload,
					payload::{
						OpAttributes,
						builder::{ExecutionInfo, OpPayloadBuilderCtx},
					},
				},
				primitives::{OpPrimitives, OpReceipt, OpTransactionSigned},
				txpool::OpPooledTx,
			},
			payload::{
				BuiltPayload,
				PayloadBuilderAttributes,
				builder::BuildOutcomeKind,
				util::PayloadTransactions,
			},
			primitives::{Header, Recovered},
			provider::{ExecutionOutcome, StateProvider},
			revm::{State, inspector::NoOpInspector},
			transaction_pool::{
				BestTransactionsAttributes,
				PoolTransaction,
				TransactionPool,
			},
		},
		revm::database::BundleState,
	},
	std::sync::Arc,
};

/// The type that builds the payload.
///
/// Payload building for optimism is composed of several steps.
/// The first steps are mandatory and defined by the protocol.
///
/// 1. first all System calls are applied.
/// 2. After canyon the forced deployed `create2deployer` must be loaded
/// 3. all sequencer transactions are executed (part of the payload attributes)
///
/// Depending on whether the node acts as a sequencer and is allowed to include
/// additional transactions (`no_tx_pool == false`):
/// 4. include additional transactions
///
/// And finally
/// 5. build the block: compute all roots (txs, state)
pub struct FlashblockBuilder<'a, Txs>
where
	Txs: PayloadTransactions<
		Transaction: PoolTransaction<Consensus = OpTxEnvelope> + OpPooledTx,
	>,
{
	/// Yields the best transaction to include if transactions from the mempool
	/// are allowed.
	best: Box<dyn Fn(BestTransactionsAttributes) -> Txs + 'a>,
}

impl<'a, Txs> FlashblockBuilder<'a, Txs>
where
	Txs: PayloadTransactions<
		Transaction: PoolTransaction<Consensus = OpTxEnvelope> + OpPooledTx,
	>,
{
	#[allow(clippy::too_many_arguments)]
	/// Creates a new [`FlashblockBuilder`].
	pub fn new(
		best: impl Fn(BestTransactionsAttributes) -> Txs + Send + Sync + 'a,
	) -> Self {
		Self {
			best: Box::new(best),
		}
	}
}

impl<'a, Txs> FlashblockBuilder<'_, Txs>
where
	Txs: PayloadTransactions<
		Transaction: PoolTransaction<Consensus = OpTxEnvelope> + OpPooledTx,
	>,
{
	/// Builds the payload on top of the state.
	pub fn build<Pool>(
		self,
		_pool: Pool,
		db: impl Database<Error = ProviderError>,
		state_provider: impl StateProvider,
		ctx: &OpPayloadBuilderCtx<OpEvmConfig, OpChainSpec>,
		committed_payload: Option<OpBuiltPayload>,
	) -> Result<BuildOutcomeKind<OpBuiltPayload>, PayloadBuilderError>
	where
		Pool: TransactionPool,
		Txs: PayloadTransactions<
			Transaction: PoolTransaction<Consensus = OpTxEnvelope> + OpPooledTx,
		>,
	{
		let Self { best } = self;
		let span = tracing::span!(
				tracing::Level::INFO,
				"flashblock_builder",
				id = %ctx.payload_id(),
		);

		let _enter = span.enter();

		tracing::debug!(target: "flashblocks::payload_builder", "building new payload");

		// 1. Prepare the db
		let (bundle, receipts, transactions, gas_used, fees) = if let Some(
			payload,
		) =
			&committed_payload
		{
			// if we have a best payload we will always have a bundle
			let execution_result = &payload
				.executed_block()
				.ok_or(PayloadBuilderError::MissingPayload)?
				.execution_output;

			let receipts = execution_result
				.receipts
				.iter()
				.flatten()
				.cloned()
				.collect();

			let transactions = payload
				.block()
				.body()
				.transactions_iter()
				.cloned()
				.map(|tx| {
					tx.try_into_recovered().map_err(|_| {
						PayloadBuilderError::Other(eyre!("tx recovery failed").into())
					})
				})
				.collect::<Result<Vec<_>, _>>()?;

			tracing::trace!(target: "flashblocks::payload_builder", "using best payload");

			(
				execution_result.bundle.clone(),
				receipts,
				transactions,
				Some(payload.block().gas_used()),
				payload.fees(),
			)
		} else {
			(BundleState::default(), vec![], vec![], None, U256::ZERO)
		};

		let _gas_limit = ctx
			.attributes()
			.gas_limit
			.unwrap_or(ctx.parent().gas_limit)
			.saturating_sub(gas_used.unwrap_or(0));

		let mut state = State::builder()
			.with_database(db)
			.with_bundle_prestate(bundle)
			.with_bundle_update()
			.build();

		// 2. Create the block builder
		let mut builder = Self::block_builder::<_, OpPrimitives, Txs>(
			&mut state,
			transactions.clone(),
			receipts,
			gas_used,
			ctx,
		)?;

		// Only execute the sequencer transactions on the first payload. The
		// sequencer transactions will already be in the [`BundleState`] at this
		// point if the `best_payload` is set.
		let mut info = if committed_payload.is_none() {
			// 3. apply pre-execution changes
			builder.apply_pre_execution_changes()?;

			// 4. Execute Deposit transactions
			ctx
				.execute_sequencer_transactions(&mut builder)
				.map_err(PayloadBuilderError::other)?
		} else {
			// bundle is non-empty - execute any transactions from the attributes that
			// do not exist on the `best_payload`
			let unexecuted_txs: Vec<Recovered<OpTxEnvelope>> = ctx
				.attributes()
				.sequencer_transactions()
				.iter()
				.filter_map(|attr_tx| {
					let tx_hash = attr_tx.1.hash();
					if !transactions.iter().any(|tx| *tx.hash() == *tx_hash) {
						Some(attr_tx.1.clone().try_into_recovered().map_err(|_| {
							PayloadBuilderError::Other(eyre!("tx recovery failed").into())
						}))
					} else {
						None
					}
				})
				.collect::<Result<Vec<_>, _>>()?;

			let mut execution_info = ExecutionInfo::default();
			let base_fee = builder.evm_mut().block().basefee;

			for tx in unexecuted_txs {
				match builder.execute_transaction(tx.clone()) {
					Ok(gas_used) => {
						execution_info.cumulative_gas_used += gas_used;
						execution_info.cumulative_da_bytes_used += tx.length() as u64;

						if !tx.is_deposit() {
							let miner_fee = tx
								.effective_tip_per_gas(base_fee)
								.expect("fee is always valid; execution succeeded");
							execution_info.total_fees +=
								U256::from(miner_fee) * U256::from(gas_used);
						}
					}

					Err(e) => {
						tracing::error!(target: "flashblocks::payload_builder", %e, "spend nullifiers transaction failed")
					}
				}
			}

			execution_info
		};

		// 5. Execute transactions from the tx-pool, draining any transactions seen
		//    in previous
		// flashblocks
		if !ctx.attributes().no_tx_pool {
			let best_txs =
				best(ctx.best_transaction_attributes(builder.evm_mut().block()));
			let mut best_txns = BestPayloadTxns::new(best_txs).with_prev(
				transactions.iter().map(|tx| *tx.hash()).collect::<Vec<_>>(),
			);

			if ctx
				.execute_best_transactions(&mut info, &mut builder, best_txns.guard())?
				.is_some()
			{
				tracing::warn!(target: "flashblocks::payload_builder", "payload build cancelled");
				if let Some(best_payload) = committed_payload {
					// we can return the previous best payload since we didn't include any
					// new txs
					return Ok(BuildOutcomeKind::Freeze(best_payload));
				} else {
					return Err(PayloadBuilderError::MissingPayload);
				}
			}

			// check if the new payload is even more valuable
			if !ctx.is_better_payload(info.total_fees + fees) {
				// can skip building the block
				return Ok(BuildOutcomeKind::Aborted {
					fees: info.total_fees + fees,
				});
			}
		}

		// 6. Build the block
		let build_outcome = builder.finish(&state_provider)?;

		// 7. Seal the block
		let BlockBuilderOutcome {
			execution_result,
			block,
			hashed_state,
			trie_updates,
		} = build_outcome;

		let sealed_block = Arc::new(block.sealed_block().clone());

		let execution_outcome = ExecutionOutcome::new(
			state.take_bundle(),
			vec![execution_result.receipts.clone()],
			block.number(),
			Vec::new(),
		);

		// create the executed block data
		let executed = BuiltPayloadExecutedBlock {
			recovered_block: Arc::new(block),
			execution_output: Arc::new(execution_outcome),
			// Keep unsorted; conversion to sorted happens when needed downstream
			hashed_state: Either::Left(Arc::new(hashed_state)),
			trie_updates: Either::Left(Arc::new(trie_updates)),
		};
		let payload = OpBuiltPayload::new(
			ctx.payload_id(),
			sealed_block,
			info.total_fees + fees,
			Some(executed),
		);

		if ctx.attributes().no_tx_pool {
			// if `no_tx_pool` is set only transactions from the payload attributes
			// will be included in the payload. In other words, the payload is
			// deterministic and we can freeze it once we've successfully built it.
			Ok(BuildOutcomeKind::Freeze(payload))
		} else {
			// always better since we are re-using built payloads
			Ok(BuildOutcomeKind::Better { payload })
		}
	}

	#[expect(clippy::type_complexity)]
	pub fn block_builder<DB, N, Tx>(
		db: &'a mut State<DB>,
		transactions: Vec<Recovered<N::SignedTx>>,
		receipts: Vec<N::Receipt>,
		cumulative_gas_used: Option<u64>,
		ctx: &'a OpPayloadBuilderCtx<OpEvmConfig, OpChainSpec>,
	) -> Result<
		FlashblocksBlockBuilder<
			'a,
			N,
			OpEvm<&'a mut State<DB>, NoOpInspector, PrecompilesMap>,
		>,
		PayloadBuilderError,
	>
	where
		N: NodePrimitives<
				Block = Block<OpTransactionSigned>,
				BlockHeader = Header,
				Receipt = OpReceipt,
			>,
		DB: Database + 'a,
		DB::Error: Send + Sync + 'static,
	{
		let attributes = OpNextBlockEnvAttributes {
			timestamp: ctx.attributes().timestamp(),
			suggested_fee_recipient: ctx.attributes().suggested_fee_recipient(),
			prev_randao: ctx.attributes().prev_randao(),
			gas_limit: ctx.attributes().gas_limit.unwrap_or(ctx.parent().gas_limit),
			parent_beacon_block_root: ctx.attributes().parent_beacon_block_root(),
			extra_data: if ctx
				.chain_spec
				.is_holocene_active_at_timestamp(ctx.attributes().timestamp())
			{
				ctx
					.attributes()
					.get_holocene_extra_data(
						ctx
							.chain_spec
							.base_fee_params_at_timestamp(ctx.attributes().timestamp()),
					)
					.map_err(PayloadBuilderError::other)?
			} else {
				Default::default()
			},
		};

		// Prepare EVM environment.
		let evm_env = ctx
			.evm_config
			.next_evm_env(ctx.parent(), &attributes)
			.map_err(PayloadBuilderError::other)?;

		// Prepare EVM.
		let evm = ctx.evm_config.evm_with_env(db, evm_env);

		// Prepare block execution context.
		let execution_ctx = ctx
			.evm_config
			.context_for_next_block(ctx.parent(), attributes)
			.map_err(PayloadBuilderError::other)?;

		let mut executor = FlashblocksBlockExecutor::new(
			evm,
			execution_ctx.clone(),
			ctx.chain_spec.clone(),
			OpRethReceiptBuilder::default(),
		)
		.with_receipts(receipts);

		if let Some(cumulative_gas_used) = cumulative_gas_used {
			executor = executor.with_gas_used(cumulative_gas_used)
		}

		Ok(FlashblocksBlockBuilder::new(
			execution_ctx,
			ctx.parent(),
			executor,
			transactions,
			ctx.chain_spec.clone(),
		))
	}
}
