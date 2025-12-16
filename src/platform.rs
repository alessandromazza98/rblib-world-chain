use {
	crate::{FlashblocksNode, context::WorldContext},
	rblib::{
		alloy::optimism::consensus::OpPooledTransaction as AlloyPoolTx,
		prelude::{
			Checkpoint,
			CheckpointExt,
			FlashbotsBundle,
			Optimism,
			PayloadBuilderError,
			Platform,
			PlatformWithRpcTypes,
			SpanExt,
			traits::{PlatformExecBounds, PlatformExecCtxBounds},
			types,
		},
		reth::{
			eth_wire_types::Encodable2718,
			optimism::{
				node::payload::{
					builder::{OpBuilder, OpPayloadBuilderCtx},
					config::OpBuilderConfig,
				},
				txpool::OpPooledTransaction,
			},
			payload::{
				builder::{BuildOutcomeKind, PayloadConfig},
				util::PayloadTransactionsFixed,
			},
			primitives::Recovered,
			providers::StateProvider,
			revm::{cancelled::CancelOnDrop, database::StateProviderDatabase},
		},
	},
	serde::{Deserialize, Serialize},
	std::sync::Arc,
};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldChain;

impl Platform for WorldChain {
	type Bundle = FlashbotsBundle<Self>;
	type CheckpointContext = WorldContext;
	type DefaultLimits = types::DefaultLimits<Optimism>;
	type EvmConfig = types::EvmConfig<Optimism>;
	type ExtraLimits = types::ExtraLimits<Optimism>;
	type NodeTypes = FlashblocksNode;
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
		tracing::info!(
			"we're entering the build_payload fn of world-chain platform",
		);
		let vanilla_built_payload = op_build_payload(payload.clone(), provider)?;
		tracing::warn!("vanilla built payload: {:?}", vanilla_built_payload);
		if let Some(latest_barrier) = payload.latest_barrier() {
			// clone is cheap because it's mostly Arc
			if let Some(built_payload) =
				latest_barrier.context().maybe_built_payload.clone()
			{
				tracing::info!(
					"we already have a built payload, we don't need to re-execute \
					 anything!",
				);
				tracing::warn!("my built payload: {:?}", built_payload);
				return Ok(built_payload);
			}
		}
		// if there are no published flashblocks yet, default to vanilla op builder
		op_build_payload(payload, provider)
	}
}

/// Inherits all optimism RPC types for the `Flashblocks` platform.
impl PlatformWithRpcTypes for WorldChain {
	type RpcTypes = types::RpcTypes<Optimism>;
}

/// This is the vanilla Optimism::build_payload fn that uses the `OpBuilder` to
/// execute transactions and create the final op built payload.
fn op_build_payload<P>(
	payload: Checkpoint<P>,
	provider: &dyn StateProvider,
) -> Result<types::BuiltPayload<P>, PayloadBuilderError>
where
	P: PlatformExecCtxBounds<WorldChain>,
{
	let block = payload.block();
	let transactions = extract_external_txs(&payload);

	let op_builder = OpBuilder::new(|_| {
		PayloadTransactionsFixed::new(
			transactions
				.into_iter()
				.map(|recovered| {
					let encoded_len = recovered.encode_2718_len();
					OpPooledTransaction::<_, AlloyPoolTx>::new(recovered, encoded_len)
				})
				.collect(),
		)
	});

	let context = OpPayloadBuilderCtx {
		evm_config: block.evm_config(),
		chain_spec: block.chainspec().clone(),
		config: PayloadConfig::<types::PayloadBuilderAttributes<P>, _>::new(
			block.parent().clone().into(),
			(*block.attributes()).clone(),
		),
		cancel: CancelOnDrop::default(),
		best_payload: None,
		builder_config: OpBuilderConfig::default(),
	};

	// Invoke the builder implementation from reth-optimism-node.
	let build_outcome =
		op_builder.build(StateProviderDatabase(&provider), &provider, context)?;

	// extract the built payload from the build outcome.
	let built_payload = match build_outcome {
		BuildOutcomeKind::Better { payload }
		| BuildOutcomeKind::Freeze(payload) => payload,
		BuildOutcomeKind::Aborted { .. } => unreachable!(
			"We are not providing the best_payload argument to the builder."
		),
		BuildOutcomeKind::Cancelled => {
			unreachable!("CancelOnDrop is not dropped in this context.")
		}
	};

	// Done!
	Ok(built_payload)
}

/// The op builder will automatically inject all transactions that are in the
/// payload attributes from the CL node. We will need to ensure that if those
/// transactions are in the transactions list, they are not duplicated and
/// removed from the transactions list provided as an argument.
///
/// This function will extract and return the list of all transactions in the
/// built payload excluding the sequencer transactions.
///
/// Payload builders might want to explicitly add those transactions during
/// the progressive payload building process to have visibility into the
/// state changes they cause and to know the cumulative gas usage including
/// those txs. This happens for example when a pipeline has a `OptimismPrologue`
/// step that applies the sequencer transactions to the payload before any other
/// step.
fn extract_external_txs<P>(
	payload: &Checkpoint<P>,
) -> Vec<Recovered<types::Transaction<WorldChain>>>
where
	P: PlatformExecCtxBounds<WorldChain>,
{
	let sequencer_txs = payload
		.block()
		.attributes()
		.transactions
		.iter()
		.map(|tx| tx.value().tx_hash());

	// all txs including potentially sequencer txs
	let full_history = payload.history();
	let transactions: Vec<_> = full_history.transactions().collect();

	let mut prefix_len = 0;
	for (ix, sequencer_tx) in sequencer_txs.enumerate() {
		if transactions
			.get(ix)
			.is_some_and(|tx| tx.tx_hash() == sequencer_tx)
		{
			prefix_len += 1;
		}
	}

	transactions
		.into_iter()
		.skip(prefix_len)
		.cloned()
		.collect::<Vec<_>>()
}
