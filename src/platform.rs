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
		_provider: &dyn StateProvider,
	) -> Result<types::BuiltPayload<P>, PayloadBuilderError>
	where
		P: PlatformExecCtxBounds<Self>,
	{
		// TODO: handle the case where there's no barrier with an
		// op built payload inside the context
		let latest_barrier = payload.latest_barrier().unwrap();
		// clone is cheap because it's mostly Arc
		let built_payload = latest_barrier
			.context()
			.maybe_built_ctx
			.as_ref()
			.unwrap()
			.clone();
		Ok(built_payload)
	}
}

/// Inherits all optimism RPC types for the `Flashblocks` platform.
impl PlatformWithRpcTypes for WorldChain {
	type RpcTypes = types::RpcTypes<Optimism>;
}
