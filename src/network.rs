use {
	crate::flashblocks::{
		ctx::OpPayloadBuilderCtxBuilder,
		p2p::FlashblocksP2PProtocol,
		state::FlashblocksStateExecutor,
	},
	rblib::reth::{
		api::{FullNodeTypes, NodeTypes, PrimitivesTy, TxTy},
		builder::{BuilderContext, components::NetworkBuilder},
		chainspec::Hardforks,
		eth_wire_types::NetPrimitivesFor,
		ethereum::network::api::FullNetwork,
		network::{NetworkProtocols, protocol::IntoRlpxSubProtocol},
		optimism::{
			chainspec::OpChainSpec,
			evm::{OpEvmConfig, OpRethReceiptBuilder},
			forks::OpHardforks,
			node::payload::builder::OpPayloadBuilderCtx,
		},
		transaction_pool::{PoolTransaction, TransactionPool},
	},
};

#[derive(Debug)]
pub struct FlashblocksNetworkBuilder<T> {
	inner: T,
	flashblocks_state: Option<FlashblocksStateExecutor>,
}

impl<T> FlashblocksNetworkBuilder<T> {
	pub fn new(
		inner: T,
		flashblocks_state: Option<FlashblocksStateExecutor>,
	) -> Self {
		Self {
			inner,
			flashblocks_state,
		}
	}
}

impl<T, Network, Node, Pool> NetworkBuilder<Node, Pool>
	for FlashblocksNetworkBuilder<T>
where
	T: NetworkBuilder<Node, Pool, Network = Network>,
	Node: FullNodeTypes<Types: NodeTypes<ChainSpec = OpChainSpec>>,
	Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
		+ Unpin
		+ 'static,
	Network: NetworkProtocols
		+ FullNetwork<Primitives: NetPrimitivesFor<PrimitivesTy<Node::Types>>>,
{
	type Network = T::Network;

	async fn build_network(
		self,
		ctx: &BuilderContext<Node>,
		pool: Pool,
	) -> eyre::Result<Self::Network> {
		let handle = self.inner.build_network(ctx, pool.clone()).await?;
		if let Some(flashblocks_state) = self.flashblocks_state {
			// Add rlpx subprotocol to handle flashblocks p2p
			let flashblocks_handle = flashblocks_state.p2p_handle();
			let flashblocks_rlpx =
				FlashblocksP2PProtocol::new(handle.clone(), flashblocks_handle);
			handle.add_rlpx_sub_protocol(flashblocks_rlpx.into_rlpx_sub_protocol());

			// Listen for flashblocks received by p2p and process them
			let evm_config =
				OpEvmConfig::new(ctx.chain_spec(), OpRethReceiptBuilder::default());
			let op_payload_builder_ctx_builder =
				OpPayloadBuilderCtxBuilder::default();
			// let op_payload_builder_ctx = OpPayloadBuilderCtx {
			// 	evm_config,
			// 	builder_config: flashblocks_state.builder_config().clone(),
			// 	chain_spec: ctx.chain_spec(),
			// 	config,
			// 	cancel: cancel.clone(),
			// 	best_payload,
			// };
			flashblocks_state.launch(
				ctx,
				pool,
				op_payload_builder_ctx_builder,
				evm_config,
			);

			// TODO: eventually uncomment this👇
			// // Merge trusted peers from both CLI args and reth.toml config file
			// let cli_peers = ctx.config().network.trusted_peers.iter();
			// let toml_peers = ctx.reth_config().peers.trusted_nodes.iter();
			// let all_trusted_peers = cli_peers.chain(toml_peers).map(|peer|
			// peer.id);

			// PeerMonitor::new(handle.clone())
			// 	.with_initial_peers(all_trusted_peers)
			// 	.run_on_task_executor(ctx.task_executor());
		}

		Ok(handle)
	}
}
