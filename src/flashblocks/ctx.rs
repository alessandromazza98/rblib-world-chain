use rblib::{
	alloy::optimism::consensus::OpTxEnvelope,
	reth::{
		api::NodePrimitives,
		evm::ConfigureEvm,
		optimism::{
			chainspec::OpChainSpec,
			evm::OpEvmConfig,
			node::{
				OpBuiltPayload,
				OpPayloadBuilderAttributes,
				payload::{builder::OpPayloadBuilderCtx, config::OpBuilderConfig},
			},
		},
		payload::builder::PayloadConfig,
		provider::ChainSpecProvider,
		revm::cancelled::CancelOnDrop,
	},
};

#[derive(Debug, Default, Clone)]
pub struct OpPayloadBuilderCtxBuilder;

impl OpPayloadBuilderCtxBuilder {
	/// Constructs a new payload builder context with the given configuration.
	///
	/// This method is called at the start of each payload building job to create
	/// a fresh context with the appropriate settings. The context encapsulates
	/// all the state and configuration needed for building a single payload.
	pub fn build<Provider>(
		&self,
		provider: Provider,
		evm: OpEvmConfig,
		builder_config: OpBuilderConfig,
		config: PayloadConfig<
            OpPayloadBuilderAttributes<OpTxEnvelope>,
            <<OpEvmConfig as ConfigureEvm>::Primitives as NodePrimitives>::BlockHeader,
        >,
		cancel: &CancelOnDrop,
		best_payload: Option<OpBuiltPayload>,
	) -> OpPayloadBuilderCtx<OpEvmConfig, OpChainSpec>
	where
		Self: Sized,
		Provider: ChainSpecProvider<ChainSpec = OpChainSpec>,
	{
		OpPayloadBuilderCtx {
			evm_config: evm,
			builder_config,
			chain_spec: provider.chain_spec(),
			config,
			cancel: cancel.clone(),
			best_payload,
		}
	}
}
