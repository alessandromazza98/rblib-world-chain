use {
	crate::{
		config::{WorldChainArgs, WorldChainNodeConfig},
		flashblocks::{p2p::FlashblocksHandle, state::FlashblocksStateExecutor},
		network::FlashblocksNetworkBuilder,
	},
	rblib::reth::{
		api::{FullNodeTypes, NodeTypes},
		builder::{
			Node, NodeAdapter, NodeComponentsBuilder,
			components::{BasicPayloadServiceBuilder, ComponentsBuilder},
			rpc::BasicEngineValidatorBuilder,
		},
		core::rpc::compat::RpcTypes,
		optimism::{
			chainspec::OpChainSpec,
			node::{
				OpAddOns, OpAddOnsBuilder, OpConsensusBuilder, OpEngineApiBuilder,
				OpEngineTypes, OpEngineValidatorBuilder, OpExecutorBuilder,
				OpNetworkBuilder, OpPoolBuilder, OpStorage, args::RollupArgs,
				node::OpPayloadBuilder, payload::config::OpBuilderConfig,
			},
			primitives::OpPrimitives,
			rpc::OpEthApiBuilder,
		},
	},
};

#[derive(Clone, Debug)]
pub struct FlashblocksNode {
	config: WorldChainNodeConfig,
	flashblocks_state: Option<FlashblocksStateExecutor>,
}

impl FlashblocksNode {
	/// Create a new `FlashblocksNode` with the provided config.
	pub fn new(config: WorldChainNodeConfig) -> Self {
		let flashblocks_handle = FlashblocksHandle::new();
		let (pending_block, _) = tokio::sync::watch::channel(None);
		let op_builder_config = OpBuilderConfig::default();
		let flashblocks_state = FlashblocksStateExecutor::new(
			flashblocks_handle.clone(),
			op_builder_config,
			pending_block,
		);
		Self {
			config,
			flashblocks_state: Some(flashblocks_state),
		}
	}

	/// Returns the flashblocks state.
	pub fn flashblocks_state(&self) -> Option<FlashblocksStateExecutor> {
		self.flashblocks_state.clone()
	}

	/// Returns an [`OpAddOnsBuilder`] with configured arguments.
	///
	/// Use this method instead of [`Node::add_ons`] when you need to modify
	/// the components (e.g., via `attach_pool`) since the builder creates
	/// add-ons that are decoupled from the original `ComponentsBuilder` type.
	pub fn add_ons_builder<NetworkT: RpcTypes>(
		&self,
	) -> OpAddOnsBuilder<NetworkT> {
		OpAddOnsBuilder::default()
			.with_sequencer(self.config.args.rollup.sequencer.clone())
			.with_sequencer_headers(self.config.args.rollup.sequencer_headers.clone())
			.with_enable_tx_conditional(self.config.args.rollup.enable_tx_conditional)
			.with_min_suggested_priority_fee(
				self.config.args.rollup.min_suggested_priority_fee,
			)
			.with_historical_rpc(self.config.args.rollup.historical_rpc.clone())
	}
}

impl<N> Node<N> for FlashblocksNode
where
	N: FullNodeTypes<Types = Self>,
{
	type AddOns = OpAddOns<
		NodeAdapter<
			N,
			<Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components,
		>,
		// TODO: change the op eth api builder with `FlashblocksEthApiBuilder`
		OpEthApiBuilder,
		OpEngineValidatorBuilder,
		OpEngineApiBuilder<OpEngineValidatorBuilder>,
		BasicEngineValidatorBuilder<OpEngineValidatorBuilder>,
	>;
	type ComponentsBuilder = ComponentsBuilder<
		N,
		// TODO: change the pool builder to account for pbh stuff
		OpPoolBuilder,
		BasicPayloadServiceBuilder<OpPayloadBuilder>,
		// TODO: change the inner type to `WorldChainNetworkBuilder`
		FlashblocksNetworkBuilder<OpNetworkBuilder>,
		OpExecutorBuilder,
		OpConsensusBuilder,
	>;

	fn components_builder(&self) -> Self::ComponentsBuilder {
		let Self {
			config:
				WorldChainNodeConfig {
					args:
						WorldChainArgs {
							rollup,
							tx_peers: _,
							flashblocks: _,
						},
				},
			flashblocks_state,
		} = self.clone();
		let RollupArgs {
			disable_txpool_gossip,
			compute_pending_block,
			discovery_v4,
			..
		} = rollup;
		let inner = OpNetworkBuilder::new(disable_txpool_gossip, !discovery_v4);
		let fb_network_builder =
			FlashblocksNetworkBuilder::new(inner, flashblocks_state.clone());
		ComponentsBuilder::default()
			.node_types::<N>()
			.pool(OpPoolBuilder::default())
			.executor(OpExecutorBuilder::default())
			.payload(BasicPayloadServiceBuilder::new(OpPayloadBuilder::new(
				compute_pending_block,
			)))
			.network(fb_network_builder)
			.consensus(OpConsensusBuilder::default())
	}

	fn add_ons(&self) -> Self::AddOns {
		self.add_ons_builder().build()
	}
}

impl NodeTypes for FlashblocksNode {
	type ChainSpec = OpChainSpec;
	type Payload = OpEngineTypes;
	type Primitives = OpPrimitives;
	type Storage = OpStorage;
}
