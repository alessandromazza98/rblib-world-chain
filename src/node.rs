use {
	crate::{
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
				node::OpPayloadBuilder,
			},
			primitives::OpPrimitives,
			rpc::OpEthApiBuilder,
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
