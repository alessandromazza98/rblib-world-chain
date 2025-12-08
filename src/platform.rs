use {
	crate::context::WorldContext,
	alloy_op_evm::{
		OpEvm,
		block::{OpAlloyReceiptBuilder, receipt_builder::OpReceiptBuilder},
	},
	rblib::{
		alloy::{
			consensus::{
				Block,
				BlockBody,
				EMPTY_OMMER_ROOT_HASH,
				Eip658Value,
				Receipt,
				Transaction,
				TxReceipt,
				proofs,
			},
			eips::merge::BEACON_NONCE,
			optimism::consensus::{OpDepositReceipt, OpReceiptEnvelope},
			primitives::U256,
		},
		prelude::{
			BlockExt,
			Checkpoint,
			CheckpointExt,
			FlashbotsBundle,
			Optimism,
			PayloadBuilderError,
			Platform,
			PlatformWithRpcTypes,
			traits::{PlatformExecBounds, PlatformExecCtxBounds},
			types,
		},
		reth::{
			errors::BlockExecutionError,
			evm::eth::receipt_builder::ReceiptBuilderCtx,
			optimism::{
				node::OpBuiltPayload,
				primitives::{OpPrimitives, OpReceipt, OpTxType},
			},
			primitives::{Header, RecoveredBlock, logs_bloom},
			provider::ExecutionOutcome,
			providers::StateProvider,
			revm::{DatabaseRef, interpreter::gas, state::EvmState},
		},
		revm::database::BundleState,
	},
	reth_chain_state::ExecutedBlock,
	reth_optimism_consensus::calculate_receipt_root_no_memo_optimism,
	serde::{Deserialize, Serialize},
	std::sync::Arc,
	world_chain_node::{context::FlashblocksContext, node::WorldChainNode},
};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldChain;

impl Platform for WorldChain {
	type Bundle = FlashbotsBundle<Self>;
	type CheckpointContext = WorldContext;
	type DefaultLimits = types::DefaultLimits<Optimism>;
	type EvmConfig = types::EvmConfig<Optimism>;
	type ExtraLimits = types::ExtraLimits<Optimism>;
	type NodeTypes = WorldChainNode<FlashblocksContext>;
	type PooledTransaction = types::PooledTransaction<Optimism>;

	fn evm_config<P>(chainspec: Arc<types::ChainSpec<P>>) -> Self::EvmConfig
	where
		P: PlatformExecBounds<Self>,
	{
		Optimism::evm_config::<Self>(chainspec)
	}

	fn next_block_environment_context<P>(
		chainspec: &types::ChainSpec<P>,
		parent: &types::Header<P>,
		attributes: &types::PayloadBuilderAttributes<P>,
	) -> Result<types::NextBlockEnvContext<P>, types::EvmEnvError<P>>
	where
		P: PlatformExecBounds<Self>,
	{
		Optimism::next_block_environment_context::<Self>(
			chainspec, parent, attributes,
		)
	}

	fn build_payload<P>(
		payload: Checkpoint<P>,
		provider: &dyn StateProvider,
	) -> Result<types::BuiltPayload<P>, PayloadBuilderError>
	where
		P: PlatformExecCtxBounds<Self>,
	{
		let base_fee = payload.block().base_fee();
		let mut total_fees = 0;
		let mut receipts: Vec<OpReceipt> = vec![];
		let mut cumulative_gas_used = 0;
		let mut bundle_state = BundleState::default();
		for checkpoint in payload.history().iter() {
			let checkpoint_bundle_state = checkpoint.state().unwrap().clone();
			bundle_state.extend(checkpoint_bundle_state);
			// cumulative_gas_used += checkpoint.gas_used();
			let checkpoint_txs = checkpoint.transactions();
			for (i, tx) in checkpoint_txs.iter().enumerate() {
				let result = checkpoint.result().unwrap().results()[i].clone();
				let gas_used = result.gas_used();
				// update and add to total fees
				let miner_fee = tx
					.effective_tip_per_gas(base_fee)
					.expect("fee is always valid; execution succeeded");
				total_fees += miner_fee * gas_used as u128;
				cumulative_gas_used += gas_used;
				let receipt = match tx.tx_type() {
					OpTxType::Deposit => {
						let receipt = Receipt {
							// Success flag was added in `EIP-658: Embedding transaction
							// status code in receipts`.
							status: Eip658Value::Eip658(result.is_success()),
							cumulative_gas_used,
							logs: result.into_logs(),
						};
						// Fetch the depositor account from the database for the deposit
						// nonce. Note that this *only* needs to be done post-regolith
						// hardfork, as deposit nonces were not introduced in Bedrock.
						// In addition, regular transactions don't have deposit
						// nonces, so we don't need to touch the DB for those.
						let depositor = checkpoint
							.basic_ref(tx.signer())
							.map_err(BlockExecutionError::other)?;
						let inner = OpDepositReceipt {
							inner: receipt,
							deposit_nonce: depositor.map(|account| account.nonce),
							// The deposit receipt version was introduced in Canyon to
							// indicate an update to how receipt hashes should be
							// computed when set. The state transition process ensures
							// this is only set for post-Canyon deposit
							// transactions.
							// TODO: fix this to account for canyon hardfork
							deposit_receipt_version: Some(1),
						};
						OpReceipt::Deposit(inner)
					}
					ty => {
						let receipt = Receipt {
							status: Eip658Value::Eip658(result.is_success()),
							cumulative_gas_used,
							logs: result.into_logs(),
						};

						match ty {
							OpTxType::Legacy => OpReceipt::Legacy(receipt),
							OpTxType::Eip2930 => OpReceipt::Eip2930(receipt),
							OpTxType::Eip1559 => OpReceipt::Eip1559(receipt),
							OpTxType::Eip7702 => OpReceipt::Eip7702(receipt),
							OpTxType::Deposit => unreachable!(),
						}
					}
				};
				receipts.push(receipt);
			}
		}
		let timestamp = payload.block().timestamp();
		let chain_spec = payload.block().chainspec();
		let txs = payload.transactions();
		let transactions_root = proofs::calculate_transaction_root(&txs);
		let receipts_root = calculate_receipt_root_no_memo_optimism(
			&receipts,
			&chain_spec,
			timestamp,
		);
		let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));
		let requests_hash = None;
		let withdrawals_root = None;
		let excess_blob_gas = None;
		let blob_gas_used = None;
		// compute state root and trie updates
		let hashed_state = provider.hashed_post_state(&bundle_state);
		let (state_root, trie_updates) = provider
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
			withdrawals: None,
		});
		let block = RecoveredBlock::new_unhashed(block, senders);
		let sealed_block = Arc::new(block.sealed_block().clone());
		let execution_outcome = ExecutionOutcome::new(
			bundle_state,
			vec![receipts],
			payload.block().number(),
			Vec::new(),
		);
		// create the executed block data
		let executed: ExecutedBlock<OpPrimitives> = ExecutedBlock {
			recovered_block: Arc::new(block),
			execution_output: Arc::new(execution_outcome),
			hashed_state: Arc::new(hashed_state),
			trie_updates: Arc::new(trie_updates),
		};
		let payload = OpBuiltPayload::new(
			payload.block().payload_id(),
			sealed_block,
			U256::from(total_fees),
			Some(executed),
		);
		Ok(payload)
	}
}

/// Inherits all optimism RPC types for the `Flashblocks` platform.
impl PlatformWithRpcTypes for WorldChain {
	type RpcTypes = types::RpcTypes<Optimism>;
}
