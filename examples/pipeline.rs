use {
	core::time::Duration,
	rblib::{
		pool::{AppendOrders, OrderPool},
		prelude::{Behavior::Loop, Minus, Pipeline, PipelineBuilderExt, Scaled},
		steps::{BreakAfterDeadline, OptimismPrologue},
	},
	rblib_world_chain::{FlashblockLimits, PublishFlashblock, WorldChain},
};

fn main() -> Result<(), std::io::Error> {
	// How often flashblocks are produced within a payload job.
	let interval = Duration::from_millis(250);

	// Flashblocks builder will always take as long as the payload job deadline,
	// this value specifies how much buffer we want to give between flashblocks
	// building and the payload job deadline that is given by the CL.
	let total_building_time = Minus(Duration::from_millis(200));

	let pool = OrderPool::<WorldChain>::default();

	// Build the flashblocks pipeline.
	let _ = Pipeline::<WorldChain>::default()
		.with_step(OptimismPrologue)
		.with_pipeline(
			Loop, // outer loop, entire payload job
			Pipeline::default()
				.with_pipeline(
					Loop, // inner loop, individual flashblocks
					(AppendOrders::from_pool(&pool), BreakAfterDeadline)
						.with_epilogue(PublishFlashblock::to())
						.with_limits(FlashblockLimits::with_interval(interval)),
				)
				.with_step(BreakAfterDeadline)
				.with_limits(Scaled::default().deadline(total_building_time)),
		);

	Ok(())
}
