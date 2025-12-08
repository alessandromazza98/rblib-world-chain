use rblib::prelude::{CheckpointContext, Platform};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorldContext {
	pub num: u64,
}

impl<P: Platform> CheckpointContext<P> for WorldContext {}

impl WorldContext {
	pub fn num(&self) -> u64 {
		self.num
	}
}
