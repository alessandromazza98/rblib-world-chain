use rblib::{
	prelude::{CheckpointContext, Platform},
	reth::optimism::node::OpBuiltPayload,
};

#[derive(Debug, Default, Clone)]
pub struct WorldContext {
	pub maybe_built_ctx: Option<OpBuiltPayload>,
}

impl PartialEq for WorldContext {
	fn eq(&self, other: &Self) -> bool {
		match (&self.maybe_built_ctx, &other.maybe_built_ctx) {
			(None, Some(_)) => false,
			(Some(_), None) => false,
			(None, None) => true,
			(Some(one), Some(other)) => one.block().hash() == other.block().hash(),
		}
	}
}

impl Eq for WorldContext {}

impl<P: Platform> CheckpointContext<P> for WorldContext {}

impl WorldContext {
	/// Create a `WorldContext` with the provided op built payload.
	pub fn new(op_built_payload: OpBuiltPayload) -> Self {
		Self {
			maybe_built_ctx: Some(op_built_payload),
		}
	}
}
