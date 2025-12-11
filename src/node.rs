use {
	crate::{
		config::{WorldChainArgs, WorldChainNodeConfig},
		flashblocks::state::FlashblocksStateExecutor,
		network::FlashblocksNetworkBuilder,
	},
	rblib::reth::{
		api::{FullNodeTypes, NodeTypes},
		builder::{
			Node,
			NodeAdapter,
			NodeComponentsBuilder,
			components::{BasicPayloadServiceBuilder, ComponentsBuilder},
			rpc::BasicEngineValidatorBuilder,
		},
		optimism::{
			chainspec::OpChainSpec,
			node::{
				OpAddOns,
				OpConsensusBuilder,
				OpEngineApiBuilder,
				OpEngineTypes,
				OpEngineValidatorBuilder,
				OpExecutorBuilder,
				OpNetworkBuilder,
				OpPoolBuilder,
				OpStorage,
				args::RollupArgs,
				node::OpPayloadBuilder,
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
		let fb_network_builder = FlashblocksNetworkBuilder::new(
			inner,
			flashblocks_state
				.as_ref()
				.map(|flashhblocks_state| flashhblocks_state.p2p_handle()),
		);
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
		todo!()
	}
}

impl NodeTypes for FlashblocksNode {
	type ChainSpec = OpChainSpec;
	type Payload = OpEngineTypes;
	type Primitives = OpPrimitives;
	type Storage = OpStorage;
}
