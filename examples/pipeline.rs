use {
	clap::Parser,
	core::time::Duration,
	rblib::{
		pool::{AppendOrders, HostNodeInstaller, OrderPool},
		prelude::{
			Behavior::Loop,
			Minus,
			Pipeline,
			PipelineBuilderExt,
			Scaled,
			types,
		},
		reth::{
			builder::Node,
			optimism::{
				cli::{Cli, chainspec::OpChainSpecParser},
				node::{OpAddOns, OpEngineApiBuilder, OpEngineValidatorBuilder},
				rpc::OpEthApiBuilder,
			},
		},
		steps::{BreakAfterDeadline, OptimismPrologue},
	},
	rblib_world_chain::{
		FlashblockLimits,
		FlashblocksNode,
		PublishFlashblock,
		WorldChain,
		WorldChainArgs,
	},
};

fn main() {
	if let Err(err) = Cli::<OpChainSpecParser, WorldChainArgs>::parse().run(
		|builder, args| async move {
			// How often flashblocks are produced within a payload job.
			let interval = Duration::from_millis(200);
			// Flashblocks builder will always take as long as the payload job
			// deadline, this value specifies how much buffer we want to give
			// between flashblocks building and the payload job deadline that is
			// given by the CL.
			let total_building_time = Minus(Duration::from_millis(100));
			// Create the pool
			let pool = OrderPool::<WorldChain>::default();
			// Create the node
			let config = args.into_config()?;
			let node = FlashblocksNode::new(config);
			// Create add ons using the builder so the type is decoupled from
			// the original ComponentsBuilder (which changes after attach_pool).
			let add_ons: OpAddOns<
				_,
				OpEthApiBuilder,
				OpEngineValidatorBuilder,
				OpEngineApiBuilder<OpEngineValidatorBuilder>,
			> = node
				.add_ons_builder::<types::RpcTypes<WorldChain>>()
				.build();
			// Build the flashblocks pipeline.
			let publish_flashlock =
				PublishFlashblock::new(node.flashblocks_state().unwrap());
			let pipeline = Pipeline::<WorldChain>::default()
				.with_step(OptimismPrologue)
				.with_pipeline(
					Loop, // outer loop, entire payload job
					Pipeline::default()
						.with_pipeline(
							Loop, // inner loop, individual flashblocks
							(
								AppendOrders::from_pool(&pool).with_ok_on_limit(),
								BreakAfterDeadline,
							)
								.with_epilogue(publish_flashlock)
								.with_limits(FlashblockLimits::with_interval(interval)),
						)
						.with_step(BreakAfterDeadline)
						.with_limits(Scaled::default().deadline(total_building_time)),
				);
			let handle = builder
				.with_types::<FlashblocksNode>()
				.with_components(
					node
						.components_builder()
						.attach_pool(&pool)
						.payload(pipeline.into_service()),
				)
				.with_add_ons(add_ons)
				.launch()
				.await?;
			handle.wait_for_node_exit().await
		},
	) {
		eprintln!("Error: {err:?}");
		std::process::exit(1);
	}
}
