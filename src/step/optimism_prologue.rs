use {
	crate::WorldChain,
	rblib::{
		Variant,
		prelude::{
			Checkpoint,
			CheckpointExt,
			ControlFlow,
			ExecutionError,
			PayloadBuilderError,
			SpanExt,
			Step,
			StepContext,
			ext::CheckpointOpExt,
		},
	},
	std::sync::Arc,
};

#[derive(Debug, thiserror::Error)]
enum OptimismPrologueError {
	#[error(
		"Optimism sequencer transactions must be at the top of the block. This \
		 payload already has {0} transaction(s)."
	)]
	TransactionsNotEmpty(usize),
	#[error("Failed to apply sequencer transaction {0}: {1:?}")]
	TransactionExecution(String, ExecutionError<WorldChain>),
	#[error("Sequencer transactions exceed block gas limit: {0} > {1})")]
	GasLimit(u64, u64),
	#[error(
		"Sequencer transactions exceed data availability bytes limit: {0} > {1}"
	)]
	DataAvailability(u64, u64),
}

/// This step appends the sequencer transactions that are defined in the payload
/// attributes parameter from the CL node into the payload under construction.
/// It requires that the payload has no existing transactions in it as the
/// sequencer expects its transactions to be at the top of the block.
pub struct OptimismPrologue;
impl Step<WorldChain> for OptimismPrologue {
	async fn step(
		self: Arc<Self>,
		payload: Checkpoint<WorldChain>,
		ctx: StepContext<WorldChain>,
	) -> ControlFlow<WorldChain> {
		// First check if we have a pre state:
		// - if we have a pre state, we don't need to execute sequencer transactions
		//   because they're already included in the pre state. Immediately return
		//   this payload.
		// - if not, execute sequencer transactions
		if let Some(latest_barrier) = payload.latest_barrier() {
			let world_ctx = latest_barrier.context();
			if world_ctx.maybe_built_payload.is_some() {
				return ControlFlow::Ok(payload);
			}
		}
		let existing_transactions_count = payload.history().transactions().count();
		if existing_transactions_count > 0 {
			return PayloadBuilderError::other(
				OptimismPrologueError::TransactionsNotEmpty(
					existing_transactions_count,
				),
			)
			.into();
		}

		let mut gas_used = 0;
		let mut da_bytes_used = 0;

		// Apply all sequencer transactions to the payload.
		let mut payload = payload;
		for tx in &ctx.block().attributes().transactions {
			let tx_value = tx.clone().into_value();
			payload = match payload.apply::<Variant<7>>(tx_value) {
				Ok(payload) => payload,
				Err(e) => {
					return PayloadBuilderError::other(
						OptimismPrologueError::TransactionExecution(format!("{tx:?}"), e),
					)
					.into();
				}
			};

			// ensure that sequencer transactions do not exceed limits.
			// If this happens, fail the whole pipeline because we will never be able
			// to build a valid payload that fits the gas limit.
			gas_used += payload.gas_used();
			if gas_used > ctx.limits().gas_limit {
				return PayloadBuilderError::other(OptimismPrologueError::GasLimit(
					gas_used,
					ctx.limits().gas_limit,
				))
				.into();
			}
			da_bytes_used += payload.da_bytes_used();
			if let Some(max_block_da) = ctx.limits().ext.max_block_da
				&& da_bytes_used > max_block_da
			{
				return PayloadBuilderError::other(
					OptimismPrologueError::DataAvailability(da_bytes_used, max_block_da),
				)
				.into();
			}
		}

		// if there were sequencer transactions added to the payload, place a
		// barrier after them, so they won't be reordered or modified by subsequent
		// steps.
		if payload.depth() > 0 {
			payload = payload.barrier();
		}

		ControlFlow::Ok(payload)
	}
}
