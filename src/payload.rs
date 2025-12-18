use {
	crate::{WorldChain, flashblocks::state::FlashblocksStateExecutor},
	rblib::{
		prelude::{
			InitContext,
			JobGenerator,
			Payload,
			PipelineServiceBuilder,
			Platform,
			ServiceContext,
			traits,
			types,
		},
		reth::{
			builder::{BuilderContext, components::PayloadServiceBuilder},
			payload::{PayloadBuilderHandle, PayloadBuilderService},
			provider::StateProviderFactory,
		},
	},
	reth_chain_state::CanonStateSubscriptions,
	std::sync::Arc,
};

/// Temporary solution to cache the flashblock payload received from p2p so that
/// it won't be re-executed when the same payload will be received by the
/// op-node thorugh a newEngine engine api message.
pub struct WorldChainServiceBuilder<P: Platform> {
	inner: PipelineServiceBuilder<P>,
	state_executor: Option<FlashblocksStateExecutor>,
}

impl<P: Platform> WorldChainServiceBuilder<P> {
	/// Create a new WorldChainServiceBuilder with the provided inner pipeline
	/// service builder and flashblocks state executor.
	pub fn new(
		inner: PipelineServiceBuilder<P>,
		state_executor: FlashblocksStateExecutor,
	) -> Self {
		Self {
			inner,
			state_executor: Some(state_executor),
		}
	}
}

impl<Plat, Node, Pool> PayloadServiceBuilder<Node, Pool, types::EvmConfig<Plat>>
	for WorldChainServiceBuilder<Plat>
where
	Plat: Platform<NodeTypes = types::NodeTypes<WorldChain>>,
	Node: traits::NodeBounds<Plat>,
	Pool: traits::PoolBounds<Plat>,
{
	async fn spawn_payload_builder_service(
		self,
		ctx: &BuilderContext<Node>,
		_: Pool,
		_: types::EvmConfig<Plat>,
	) -> eyre::Result<PayloadBuilderHandle<types::PayloadTypes<Plat>>> {
		let pipeline = self.inner.pipeline();
		tracing::debug!("Spawning payload builder service for: {pipeline:#?}");

		let provider = Arc::new(ctx.provider().clone());

		let service = ServiceContext {
			provider: Arc::clone(&provider),
			node_config: ctx.config().clone(),
			metrics: Payload::with_scope(&format!("{}_payloads", pipeline.name())),
		};

		// assign metric names to each step in the pipeline.
		// This will be used to automatically generate runtime observability data
		// during pipeline runs. Also call an optional `setup` function on the step
		// that gives it a chance to perform any initialization it needs before
		// processing any payload jobs.
		let provider = provider as Arc<dyn StateProviderFactory>;

		for step in pipeline.iter_steps() {
			let navi = step.navigator(&pipeline).expect(
				"Invalid step path. This is a bug in the pipeline executor \
				 implementation.",
			);

			let metrics_scope = format!("{}_step_{}", pipeline.name(), navi.path());
			navi.instance().init_metrics(&metrics_scope);

			let init_ctx = InitContext::new(Arc::clone(&provider), metrics_scope);
			navi.instance().setup(init_ctx).await?;
		}

		let (service, builder) = PayloadBuilderService::new(
			JobGenerator::new(pipeline, service),
			ctx.provider().canonical_state_stream(),
		);

		if let Some(state_executor) = self.state_executor {
			let payload_events = service.payload_events_handle();
			state_executor.register_payload_events(payload_events);
		}

		ctx
			.task_executor()
			.spawn_critical("payload_builder_service", Box::pin(service));

		Ok(builder)
	}
}
