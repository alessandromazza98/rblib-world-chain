use {
	crate::WorldChain,
	rblib::prelude::{
		Checkpoint,
		ControlFlow,
		PayloadBuilderError,
		Step,
		StepContext,
		types,
	},
	std::sync::Arc,
};

#[derive(Debug, Default)]
pub struct PublishFlashblock {}

impl Step<WorldChain> for PublishFlashblock {
	async fn step(
		self: Arc<Self>,
		_payload: Checkpoint<WorldChain>,
		_ctx: StepContext<WorldChain>,
	) -> ControlFlow<WorldChain> {
		todo!()
	}

	async fn before_job(
		self: Arc<Self>,
		_ctx: StepContext<WorldChain>,
	) -> Result<(), PayloadBuilderError> {
		todo!()
	}

	async fn after_job(
		self: Arc<Self>,
		_ctx: StepContext<WorldChain>,
		_result: Arc<Result<types::BuiltPayload<WorldChain>, PayloadBuilderError>>,
	) -> Result<(), PayloadBuilderError> {
		todo!()
	}
}
