//! Reth node that supports ress subprotocol.

use clap::Parser;
use reth::{args::RessArgs, chainspec::EthereumChainSpecParser};
use reth_node_builder::NodeHandle;
use reth_ress_provider::{maintain_pending_state, PendingState, RethRessProtocolProvider};

fn main() -> eyre::Result<()> {
    reth::cli::Cli::<EthereumChainSpecParser, RessArgs>::parse().run(
        |builder, ress_args| async move {
            // launch the stateful node
            let NodeHandle { node, node_exit_future } =
                builder.node(reth_node_ethereum::EthereumNode::default()).launch().await?;

            // Install ress subprotocol.
            if ress_args.enabled {
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
                    ress_args.max_witness_window,
                    ress_args.witness_max_parallel,
                    ress_args.witness_cache_size,
                    pending_state,
                )?;

                reth::ress::install_ress_subprotocol(
                    provider,
                    node.network,
                    node.task_executor,
                    ress_args.max_active_connections,
                )?;
            }

            node_exit_future.await
        },
    )
}
