use {
	crate::{
		PublishFlashblock,
		flashblocks::{
			block_builder::FlashblocksBlockBuilder,
			block_executor::FlashblocksBlockExecutor,
			payload_txns::BestPayloadTxns,
		},
		flatten_reverts,
	},
	alloy_evm::{
		EvmEnv,
		EvmFactory,
		block::BlockExecutionError,
		op_revm::{OpHaltReason, OpSpecId},
		revm::{
			Database,
			context::result::ExecutionResult,
			state::bal::{AccountBal, Bal},
		},
	},
	alloy_op_evm::OpEvmFactory,
	alloy_rlp::Encodable,
	eyre::eyre,
	rblib::{
		alloy::{
			consensus::{
				Block,
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
				Decodable2718,
				eip7685::EMPTY_REQUESTS_HASH,
				eip7928::{BlockAccessList, compute_block_access_list_hash},
				merge::BEACON_NONCE,
			},
			optimism::consensus::{OpDepositReceipt, OpTxEnvelope},
			primitives::{Bytes, U256},
			signers::Either,
		},
		prelude::{IntoExecutable, PayloadBuilderError},
		reth::{
			api::{
				BlockBody as BlockBodyTrait,
				BuiltPayloadExecutedBlock,
				NodePrimitives,
			},
			core::primitives::SignedTransaction,
			errors::ProviderError,
			ethereum::trie::iter::{
				IndexedParallelIterator,
				IntoParallelIterator,
				ParallelIterator,
			},
			evm::{
				ConfigureEvm,
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
				primitives::{OpPrimitives, OpReceipt, OpTransactionSigned, OpTxType},
				txpool::OpPooledTx,
			},
			payload::{
				BuiltPayload,
				PayloadBuilderAttributes,
				builder::BuildOutcomeKind,
				util::PayloadTransactions,
			},
			primitives::{BlockBody, Header, Recovered, RecoveredBlock, logs_bloom},
			provider::{ExecutionOutcome, StateProvider},
			revm::{State, inspector::NoOpInspector},
			rpc::types::Withdrawals,
			transaction_pool::{
				BestTransactionsAttributes,
				PoolTransaction,
				TransactionPool,
			},
		},
		revm::database::{BundleState, states::bundle_state::BundleRetention},
	},
	reth_optimism_consensus::{calculate_receipt_root_no_memo_optimism, isthmus},
	std::{fmt::Debug, sync::Arc},
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
		db: impl Database<Error = ProviderError> + Clone + Debug + Send + Sync,
		state_provider: impl StateProvider,
		ctx: &OpPayloadBuilderCtx<OpEvmConfig, OpChainSpec>,
		committed_payload: Option<OpBuiltPayload>,
		bal: Option<BlockAccessList>,
		evm_env: EvmEnv<OpSpecId>,
		extra_data: Bytes,
	) -> Result<BuildOutcomeKind<OpBuiltPayload>, PayloadBuilderError>
	where
		Pool: TransactionPool,
		Txs: PayloadTransactions<
			Transaction: PoolTransaction<Consensus = OpTxEnvelope> + OpPooledTx,
		>,
	{
		let span = tracing::span!(
				tracing::Level::INFO,
				"flashblock_builder",
				id = %ctx.payload_id(),
		);

		let _enter = span.enter();

		tracing::debug!(target: "flashblocks::payload_builder", "building new payload");

		let mut transactions_offset = 0;
		// 1. Prepare the db
		let (mut bundle, mut receipts, mut transactions, gas_used, mut fees) =
			if let Some(payload) = &committed_payload {
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

				transactions_offset = (payload.block().transaction_count() + 1) as u64;
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
			.with_database(db.clone())
			.with_bundle_prestate(bundle.clone())
			.with_bundle_update()
			.build();
		// if there is the block access list, then use it to parallelize transaction
		// execution
		if let Some(bal) = bal {
			let mut bal_vec = vec![];
			for alloy_acc in bal {
				let (addr, acc_bal) = AccountBal::try_from_alloy(alloy_acc)
					.expect("it should not fail here");
				bal_vec.push((addr, acc_bal));
			}
			let bal = Bal::from_iter(bal_vec);
			// TODO: bal_builder needs to account for potential committed state that
			// has already started created a block access list with previous
			// flashblocks
			state.bal_state.bal_builder = Some(Bal::default());
			state.bal_state.bal = Some(Arc::new(bal));
			state.bal_state.bal_index = transactions_offset;
		}

		// 2. Create the block builder
		let mut builder = Self::block_builder::<_, OpPrimitives, Txs>(
			&mut state,
			transactions.clone(),
			receipts.clone(),
			gas_used,
			ctx,
		)?;

		if transactions_offset == 0 {
			// 3. apply pre-execution changes
			builder.apply_pre_execution_changes()?;
			transactions_offset += 1;
		}
		let bal_state_cloned = state.bal_state.clone();
		let attributes_txs: Vec<Recovered<OpTxEnvelope>> = ctx
			.attributes()
			.transactions
			.clone()
			.into_iter()
			.map(|tx| {
				tx.1.try_into_recovered().map_err(|err| {
					PayloadBuilderError::Other(
						eyre!("tx recovery failed for {:?}", err).into(),
					)
				})
			})
			.collect::<Result<Vec<_>, _>>()?;
		let attributes_txsx_indexed: Vec<(u64, Recovered<OpTxEnvelope>)> =
			attributes_txs
				.clone()
				.into_iter()
				.enumerate()
				.map(|(idx, tx)| (idx as u64 + transactions_offset, tx))
				.collect();
		let mut parallel_results = attributes_txsx_indexed
			.into_par_iter()
			.map(|(idx, tx)| {
				let mut state_paral = State::builder()
					.with_database(db.clone())
					.with_bundle_prestate(bundle.clone())
					.with_bundle_update()
					.build();
				state_paral.bal_state = bal_state_cloned.clone();
				state_paral.set_bal_index(idx);
				let mut evm =
					OpEvmFactory::default().create_evm(&mut state_paral, evm_env.clone());
				let exec_result = evm
					.transact_commit(tx.clone())
					.map_err(ProviderError::other)?;
				state_paral.merge_transitions(BundleRetention::Reverts);
				let bundle_state = state_paral.take_bundle();
				let bal = state_paral.take_built_bal();
				let parallel_result = ParallelResult {
					index: idx,
					bundle_state,
					exec_result,
					tx,
					bal,
				};
				Ok(parallel_result)
			})
			.collect::<Result<Vec<_>, PayloadBuilderError>>()?;
		parallel_results.sort_unstable_by_key(|r| r.index);
		let mut cumulative_gas_used = gas_used.unwrap_or_default();
		let mut final_bal = Bal::default();
		for parallel_result in parallel_results {
			let parallel_bal = parallel_result.bal.unwrap_or_default();
			for (addr, acc_bal) in parallel_bal.accounts {
				final_bal
					.accounts
					.entry(addr)
					.and_modify(|account_bal| {
						account_bal
							.account_info
							.extend(acc_bal.account_info.clone());
						account_bal.storage.extend(acc_bal.storage.clone());
					})
					.or_insert(acc_bal);
			}
			bundle.extend(parallel_result.bundle_state);
			let gas_used_by_tx = parallel_result.exec_result.gas_used();
			cumulative_gas_used += gas_used_by_tx;
			let mut deposit_nonce = None;
			if parallel_result.tx.is_deposit() {
				state.set_bal_index(parallel_result.index);
				deposit_nonce = state
					.basic(parallel_result.tx.signer())
					.map_err(ProviderError::other)?
					.map(|account| account.nonce)
			} else {
				let miner_fee = parallel_result
					.tx
					.effective_tip_per_gas(evm_env.block_env.basefee)
					.expect("fee is always valid; execution succeeded");
				fees += U256::from(miner_fee) * U256::from(gas_used_by_tx);
			};
			let receipt = build_receipt(
				&parallel_result.tx,
				parallel_result.exec_result,
				cumulative_gas_used,
				deposit_nonce,
				ctx.chain_spec.as_ref(),
				ctx.attributes().timestamp(),
			)?;
			receipts.push(receipt);
		}
		transactions.extend(attributes_txs);
		let transactions_root = proofs::calculate_transaction_root(&transactions);
		let receipts_root = calculate_receipt_root_no_memo_optimism(
			&receipts,
			ctx.chain_spec.clone(),
			ctx.attributes().timestamp(),
		);
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
		let flattened = flatten_reverts(&bundle.reverts);
		bundle.reverts = flattened;
		let mut requests_hash = None;
		let withdrawals_root = if ctx
			.chain_spec
			.is_isthmus_active_at_timestamp(ctx.attributes().timestamp())
		{
			// always empty requests hash post isthmus
			requests_hash = Some(EMPTY_REQUESTS_HASH);
			// withdrawals root field in block header is used for storage root of L2
			// predeploy `l2tol1-message-passer`
			Some(
				isthmus::withdrawals_root(&bundle, &state_provider)
					.map_err(BlockExecutionError::other)?,
			)
		} else if ctx
			.chain_spec
			.is_canyon_active_at_timestamp(ctx.attributes().timestamp())
		{
			Some(EMPTY_WITHDRAWALS)
		} else {
			None
		};
		let (excess_blob_gas, blob_gas_used) = if ctx
			.chain_spec
			.is_jovian_active_at_timestamp(ctx.attributes().timestamp())
		{
			// TODO: this should depend on da footprint's value. world chain doesn't
			// use alternative DA right now so I can temporarily ignore it.
			let blob_gas_used = 0;
			// In jovian, we're using the blob gas used field to store the current
			// da footprint's value.
			(Some(0), Some(blob_gas_used))
		} else if ctx
			.chain_spec
			.is_ecotone_active_at_timestamp(ctx.attributes().timestamp())
		{
			(Some(0), Some(0))
		} else {
			(None, None)
		};
		let hashed_state = state_provider.hashed_post_state(&bundle);
		let (state_root, trie_updates) = state_provider
			.state_root_with_updates(hashed_state.clone())
			.map_err(BlockExecutionError::other)?;
		let alloy_bal = final_bal.into_alloy_bal();
		let block_access_list_hash =
			Some(compute_block_access_list_hash(&alloy_bal));
		let header = Header {
			parent_hash: ctx.parent().hash(),
			ommers_hash: EMPTY_OMMER_ROOT_HASH,
			beneficiary: evm_env.block_env.beneficiary,
			state_root,
			transactions_root,
			receipts_root,
			withdrawals_root,
			logs_bloom,
			timestamp: ctx.attributes().timestamp(),
			mix_hash: evm_env.block_env.prevrandao.unwrap_or_default(),
			nonce: BEACON_NONCE.into(),
			base_fee_per_gas: Some(evm_env.block_env.basefee),
			number: ctx.parent().number + 1,
			gas_limit: ctx.attributes().gas_limit.unwrap_or_default(),
			difficulty: evm_env.block_env.difficulty,
			gas_used: cumulative_gas_used,
			extra_data,
			parent_beacon_block_root: ctx.attributes().parent_beacon_block_root(),
			blob_gas_used,
			excess_blob_gas,
			requests_hash,
			block_access_list_hash,
		};

		let (transactions, senders) = transactions
			.iter()
			.map(|tx| tx.clone().into_parts())
			.unzip();
		let block = Block::new(header, BlockBody {
			transactions,
			ommers: Default::default(),
			withdrawals: Some(Withdrawals::default()), // empty withdrawals
			block_access_list: Some(alloy_bal),
		});
		let block = RecoveredBlock::new_unhashed(block, senders);
		let sealed_block = Arc::new(block.sealed_block().clone());
		tracing::info!("processed flashblock, block hash: {}", sealed_block.hash());
		let execution_outcome = ExecutionOutcome::new(
			bundle,
			vec![receipts],
			ctx.parent().number + 1,
			Vec::new(),
		);

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
			U256::from(fees),
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
		DB: Database + Debug + 'a,
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

#[derive(Debug)]
pub struct ParallelResult {
	index: u64,
	bundle_state: BundleState,
	exec_result: ExecutionResult<OpHaltReason>,
	tx: Recovered<OpTxEnvelope>,
	bal: Option<Bal>,
}

/// Builds an OpReceipt from a transaction execution result.
///
/// Handles the different receipt types based on transaction type (deposit vs
/// regular). For deposit transactions, includes deposit-specific fields like
/// nonce and version.
pub fn build_receipt(
	tx: &Recovered<OpTxEnvelope>,
	result: ExecutionResult<OpHaltReason>,
	cumulative_gas_used: u64,
	deposit_nonce: Option<u64>,
	chain_spec: &impl OpHardforks,
	timestamp: u64,
) -> Result<OpReceipt, ProviderError> {
	let receipt = Receipt {
		status: Eip658Value::Eip658(result.is_success()),
		cumulative_gas_used,
		logs: result.into_logs(),
	};

	match tx.tx_type() {
		OpTxType::Deposit => {
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
