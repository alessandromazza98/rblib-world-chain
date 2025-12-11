use {
	crate::{
		flashblocks::state::FlashblocksStateExecutor,
		network::FlashblocksNetworkBuilder,
	},
	rblib::reth::{
		api::{FullNodeTypes, NodeTypes},
		builder::{
			Node,
			components::{BasicPayloadServiceBuilder, ComponentsBuilder},
		},
		optimism::{
			chainspec::OpChainSpec,
			node::{
				OpConsensusBuilder,
				OpEngineTypes,
				OpExecutorBuilder,
				OpNetworkBuilder,
				OpPoolBuilder,
				OpStorage,
				node::OpPayloadBuilder,
			},
			primitives::OpPrimitives,
		},
	},
};

#[derive(Clone, Debug, Default)]
pub struct FlashblocksNode {
	flashblocks_state: Option<FlashblocksStateExecutor>,
}

impl<N> Node<N> for FlashblocksNode
where
	N: FullNodeTypes<Types = Self>,
{
	type AddOns;
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
		todo!()
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
