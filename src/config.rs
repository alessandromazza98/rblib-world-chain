use {
	clap::Args,
	rblib::reth::{network_peers::PeerId, optimism::node::args::RollupArgs},
};

#[derive(Debug, Clone)]
pub struct WorldChainNodeConfig {
	/// World Chain Specific CLI arguements
	pub args: WorldChainArgs,
}

#[derive(Debug, Clone, Args)]
pub struct WorldChainArgs {
	/// op rollup args
	#[command(flatten)]
	pub rollup: RollupArgs,

	// TODO: uncomment this👇
	// /// Pbh args
	// #[command(flatten)]
	// pub pbh: PbhArgs,

	// TODO: uncomment this👇
	// /// Builder args
	// #[command(flatten)]
	// pub builder: BuilderArgs,
	/// Flashblock args
	#[command(flatten)]
	pub flashblocks: Option<FlashblocksArgs>,

	/// Comma-separated list of peer IDs to which transactions should be
	/// propagated
	#[arg(long = "tx-peers", value_delimiter = ',', value_name = "PEER_ID")]
	pub tx_peers: Option<Vec<PeerId>>,
}

impl WorldChainArgs {
	pub fn into_config(mut self) -> eyre::Result<WorldChainNodeConfig> {
		// Perform arg validation here for things clap can't do.

		if let Some(peers) = &self.tx_peers {
			if self.rollup.disable_txpool_gossip {
				tracing::warn!(
						target: "world_chain::network",
						"--tx-peers is ignored when transaction pool gossip is disabled \
						 (--rollup.disable-tx-pool-gossip). The --tx-peers flag is shadowed and has no effect."
				);
				self.tx_peers = None;
			} else {
				tracing::info!(
						target: "world_chain::network",
						"Transaction propagation restricted to {} peer(s)",
						peers.len()
				);
			}
		}
		Ok(WorldChainNodeConfig { args: self })
	}
}

/// Flashblocks configuration
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[command(next_help_heading = "Flashblocks")]
pub struct FlashblocksArgs {
	#[arg(
		long = "flashblocks.enabled",
		id = "flashblocks.enabled",
		required = false
	)]
	pub enabled: bool,

	/// The interval to publish pre-confirmations
	/// when building a payload in milliseconds.
	#[arg(
		long = "flashblocks.interval",
		env = "FLASHBLOCKS_INTERVAL",
		default_value_t = 200
	)]
	pub flashblocks_interval: u64,

	/// Interval at which the block builder
	/// should re-commit to the transaction pool when building a payload.
	///
	/// In milliseconds.
	#[arg(
		long = "flashblocks.recommit_interval",
		env = "FLASHBLOCKS_RECOMMIT_INTERVAL",
		default_value_t = 200
	)]
	pub recommit_interval: u64,
}
