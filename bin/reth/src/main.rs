//! Reth node that supports ress subprotocol.

use clap::Parser;
use reth::{
    chainspec::EthereumChainSpecParser, ress::install_ress_subprotocol,
    zk_ress::install_zk_ress_subprotocol, ExtraArgs,
};
use reth_node_builder::NodeHandle;
use reth_ress_provider::{maintain_pending_state, PendingState, RethRessProtocolProvider};
use reth_zk_ress_provider::ZkRessProver;
use tracing::info;

fn main() -> eyre::Result<()> {
    reth::cli::Cli::<EthereumChainSpecParser, ExtraArgs>::parse().run(
        |builder, extra_args| async move {
            // launch the stateful node
            let NodeHandle { node, node_exit_future } =
                builder.node(reth_node_ethereum::EthereumNode::default()).launch().await?;

            // Install ress & zkress subprotocols.
            if extra_args.ress.enabled || !extra_args.zk_ress.protocols.is_empty() {
                info!(target: "reth::cli", "Initializing ress pending state");
                let pending_state = PendingState::default();

                // Spawn maintenance task for pending state.
                node.task_executor.spawn(maintain_pending_state(
                    node.add_ons_handle.engine_events.new_listener(),
                    node.provider.clone(),
                    pending_state.clone(),
                ));

                let provider = RethRessProtocolProvider::new(
                    node.provider.clone(),
                    node.evm_config.clone(),
                    Box::new(node.task_executor.clone()),
                    extra_args.ress.max_witness_window,
                    extra_args.ress.witness_max_parallel,
                    extra_args.ress.witness_cache_size,
                    pending_state,
                )?;

                // Install ress
                if extra_args.ress.enabled {
                    install_ress_subprotocol(
                        provider.clone(),
                        node.network.clone(),
                        node.task_executor.clone(),
                        extra_args.ress.max_active_connections,
                    )?;
                }

                // Install zk ress
                for protocol in &extra_args.zk_ress.protocols {
                    let prover = ZkRessProver::try_from_arg(protocol)?;
                    install_zk_ress_subprotocol(
                        prover,
                        0,
                        provider.clone(),
                        node.network.clone(),
                        node.task_executor.clone(),
                        extra_args.ress.max_active_connections,
                    )?;
                }
            }

            node_exit_future.await
        },
    )
}
