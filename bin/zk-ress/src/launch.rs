use alloy_network::Ethereum;
use alloy_rpc_types_engine::{ClientCode, ClientVersionV1, JwtSecret};
use futures::StreamExt;
use http::{header::CONTENT_TYPE, HeaderValue, Response};
use reth_chainspec::ChainSpec;
use reth_consensus_debug_client::{DebugConsensusClient, RpcBlockProvider};
use reth_ethereum_primitives::EthPrimitives;
use reth_network::{
    config::SecretKey, protocol::IntoRlpxSubProtocol, EthNetworkPrimitives, NetworkConfig,
    NetworkInfo, NetworkManager, PeersInfo,
};
use reth_network_peers::TrustedPeer;
use reth_node_api::BeaconConsensusEngineHandle;
use reth_node_core::primitives::{RecoveredBlock, SealedBlock};
use reth_node_ethereum::{
    consensus::EthBeaconConsensus, node::EthereumEngineValidator, EthEngineTypes,
};
use reth_node_events::node::handle_events;
use reth_node_metrics::recorder::install_prometheus_recorder;
use reth_payload_builder::{noop::NoopPayloadBuilderService, PayloadStore};
use reth_ress_protocol::NodeType;
use reth_rpc_api::EngineEthApiServer;
use reth_rpc_builder::auth::{AuthRpcModule, AuthServerConfig, AuthServerHandle};
use reth_rpc_engine_api::{capabilities::EngineCapabilities, EngineApi};
use reth_storage_api::noop::NoopProvider;
use reth_tasks::TokioTaskExecutor;
use reth_transaction_pool::noop::NoopTransactionPool;
use reth_zk_ress_protocol::{ProtocolState, ZkRessProtocolHandler, ZkRessProtocolProvider};
use reth_zk_ress_provider::ZkRessProver;
use std::{convert::Infallible, fmt, net::SocketAddr, sync::Arc};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::*;
use zk_ress_engine::engine::ConsensusEngine;
use zk_ress_network::{RessNetworkHandle, RessNetworkManager};
use zk_ress_provider::ZkRessProvider;
use zk_ress_verifier::ExecutionWitnessVerifier;

use crate::{cli::ZkRessArgs, rpc::RessEthRpc};

/// The human readable name of the client
pub const NAME_CLIENT: &str = "ZkRess";

/// The latest version from Cargo.toml.
pub const CARGO_PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The 8 character short SHA of the latest commit.
pub const VERGEN_GIT_SHA: &str = env!("VERGEN_GIT_SHA_SHORT");

/// Ress node launcher
#[derive(Debug)]
pub struct NodeLauncher {
    /// Ress configuration.
    args: ZkRessArgs,
}

impl NodeLauncher {
    /// Create new node launcher
    pub fn new(args: ZkRessArgs) -> Self {
        Self { args }
    }
}

impl NodeLauncher {
    /// Launch ress node.
    pub async fn launch(self) -> eyre::Result<()> {
        let data_dir = self.args.datadir.unwrap_or_chain_default(self.args.chain.chain());

        // Create provider.
        let provider = ZkRessProvider::new(self.args.chain.clone());

        // Install the recorder to ensure that upkeep is run periodically and
        // start the metrics server.
        install_prometheus_recorder().spawn_upkeep();
        if let Some(addr) = self.args.metrics {
            info!(target: "ress", ?addr, "Starting metrics endpoint");
            self.start_prometheus_server(addr).await?;
        }

        // Insert genesis block.
        let genesis_hash = self.args.chain.genesis_hash();
        let genesis_header = self.args.chain.genesis_header().clone();
        provider.insert_block(
            RecoveredBlock::new_sealed(
                SealedBlock::from_parts_unchecked(genesis_header, Default::default(), genesis_hash),
                Vec::new(),
            ),
            None,
        );
        provider.insert_canonical_hash(0, genesis_hash);
        info!(target: "ress", %genesis_hash, "Inserted genesis block");

        // Launch network.
        let network_secret_path = self.args.network.network_secret_path(&data_dir);
        let network_secret = reth_cli_util::get_secret_key(&network_secret_path)?;

        let network_handle = self
            .launch_network(
                provider.clone(),
                network_secret,
                self.args.network.max_active_connections,
                self.args.network.trusted_peers.clone(),
            )
            .await?;
        info!(target: "ress", peer_id = %network_handle.inner().peer_id(), addr = %network_handle.inner().local_addr(), enode = %network_handle.inner().local_node_record().to_string(), "Network launched");

        // Spawn consensus engine.
        let (to_engine, from_auth_rpc) = mpsc::unbounded_channel();
        let consensus = EthBeaconConsensus::new(self.args.chain.clone());
        let engine_validator = EthereumEngineValidator::new(self.args.chain.clone());
        let block_verifier = ExecutionWitnessVerifier::new(provider.clone());
        let (engine_events_tx, engine_events_rx) = mpsc::unbounded_channel();
        let consensus_engine = ConsensusEngine::new(
            provider.clone(),
            consensus,
            engine_validator.clone(),
            block_verifier,
            network_handle.clone(),
            from_auth_rpc,
            engine_events_tx,
        );
        let _consensus_engine_handle = tokio::spawn(consensus_engine);
        info!(target: "ress", "Consensus engine spawned");

        // Start auth RPC server.
        let jwt_key = self.args.rpc.auth_jwt_secret(data_dir.jwt())?;
        let beacon_consensus_engine_handle =
            BeaconConsensusEngineHandle::<EthEngineTypes>::new(to_engine);
        let auth_server_handle = self
            .start_auth_server(
                jwt_key,
                provider,
                engine_validator,
                beacon_consensus_engine_handle.clone(),
            )
            .await?;
        info!(target: "ress", addr = %auth_server_handle.local_addr(), "Auth RPC server started");

        // Start debug consensus.
        if let Some(url) = self.args.debug.debug_consensus_url {
            let rpc_to_primitive_block = |rpc_block: alloy_rpc_types_eth::Block| {
                let alloy_rpc_types_eth::Block { header, transactions, withdrawals, .. } =
                    rpc_block;
                reth_ethereum_primitives::Block {
                    header: header.inner,
                    body: reth_ethereum_primitives::BlockBody {
                        transactions: transactions
                            .into_transactions()
                            .map(|tx| tx.inner.into_inner().into())
                            .collect(),
                        ommers: Default::default(),
                        withdrawals,
                    },
                }
            };
            let provider = Arc::new(
                RpcBlockProvider::<Ethereum, reth_ethereum_primitives::Block>::new(
                    &url,
                    rpc_to_primitive_block,
                )
                .await?,
            );
            tokio::spawn(DebugConsensusClient::new(beacon_consensus_engine_handle, provider).run());
            info!(target: "ress", %url, "Debug consensus started");
        }

        handle_events::<_, EthPrimitives>(
            Some(Box::new(network_handle.inner().clone())),
            None,
            UnboundedReceiverStream::from(engine_events_rx).map(Into::into),
        )
        .await;

        Ok(())
    }

    async fn launch_network<P>(
        &self,
        protocol_provider: P,
        secret_key: SecretKey,
        max_active_connections: u64,
        trusted_peers: Vec<TrustedPeer>,
    ) -> eyre::Result<RessNetworkHandle<P::Proof>>
    where
        P: ZkRessProtocolProvider + Clone + Unpin + 'static,
        P::Proof: fmt::Debug,
    {
        // Configure and instantiate the network
        let config = NetworkConfig::builder(secret_key)
            .listener_addr(self.args.network.listener_addr())
            .disable_discovery()
            .build_with_noop_provider(self.args.chain.clone());
        let mut manager = NetworkManager::<EthNetworkPrimitives>::new(config).await?;

        let (events_sender, protocol_events) = mpsc::unbounded_channel();
        let protocol_handler = ZkRessProtocolHandler {
            protocol_name: ZkRessProver::ExecutionWitness.protocol_name(),
            protocol_version: 0,
            provider: protocol_provider,
            node_type: NodeType::Stateless,
            peers_handle: manager.peers_handle(),
            max_active_connections,
            state: ProtocolState { events_sender, active_connections: Arc::default() },
        };
        manager.add_rlpx_sub_protocol(protocol_handler.into_rlpx_sub_protocol());

        for trusted_peer in trusted_peers {
            let trusted_peer_addr = trusted_peer.resolve_blocking()?.tcp_addr();
            manager.peers_handle().add_peer(trusted_peer.id, trusted_peer_addr);
        }

        // get a handle to the network to interact with it
        let network_handle = manager.handle().clone();
        // spawn the network
        tokio::spawn(manager);

        let (peer_requests_tx, peer_requests_rx) = mpsc::unbounded_channel();
        let peer_request_stream = UnboundedReceiverStream::from(peer_requests_rx);
        if let Some(rpc_url) = self.args.debug.rpc_network_adapter_url.clone() {
            info!(target: "ress", url = %rpc_url, "Using RPC network adapter");
            // TODO:
            // tokio::spawn(RpcNetworkAdapter::new(&rpc_url).await?.run(peer_request_stream));
            unimplemented!()
        } else {
            // spawn ress network manager
            tokio::spawn(RessNetworkManager::new(
                UnboundedReceiverStream::from(protocol_events),
                peer_request_stream,
            ));
        }

        Ok(RessNetworkHandle::new(network_handle, peer_requests_tx))
    }

    async fn start_auth_server<T>(
        &self,
        jwt_key: JwtSecret,
        provider: ZkRessProvider<T>,
        engine_validator: EthereumEngineValidator,
        beacon_engine_handle: BeaconConsensusEngineHandle<EthEngineTypes>,
    ) -> eyre::Result<AuthServerHandle>
    where
        T: Clone + Send + Sync + 'static,
    {
        let (_, payload_builder_handle) = NoopPayloadBuilderService::<EthEngineTypes>::new();
        let client_version = ClientVersionV1 {
            code: ClientCode::RH,
            name: NAME_CLIENT.to_string(),
            version: CARGO_PKG_VERSION.to_string(),
            commit: VERGEN_GIT_SHA.to_string(),
        };
        let engine_api = EngineApi::new(
            NoopProvider::<ChainSpec, EthPrimitives>::new(self.args.chain.clone()),
            self.args.chain.clone(),
            beacon_engine_handle,
            PayloadStore::new(payload_builder_handle),
            NoopTransactionPool::default(),
            Box::<TokioTaskExecutor>::default(),
            client_version,
            EngineCapabilities::default(),
            engine_validator,
            false,
        );
        let auth_socket = self.args.rpc.auth_rpc_addr();
        let config = AuthServerConfig::builder(jwt_key).socket_addr(auth_socket).build();

        let mut module = AuthRpcModule::new(engine_api);
        module.merge_auth_methods(RessEthRpc::new(provider).into_rpc())?;
        Ok(module.start_server(config).await?)
    }

    /// This launches the prometheus server.
    pub async fn start_prometheus_server(&self, addr: SocketAddr) -> eyre::Result<()> {
        // Register version.
        let _gauge = metrics::gauge!("info", &[("version", env!("CARGO_PKG_VERSION"))]);

        let listener = tokio::net::TcpListener::bind(addr).await?;
        tokio::spawn(async move {
            loop {
                let io = match listener.accept().await {
                    Ok((stream, _remote_addr)) => stream,
                    Err(error) => {
                        tracing::error!(target: "ress", %error, "failed to accept connection");
                        continue;
                    }
                };

                let handle = install_prometheus_recorder();
                let service = tower::service_fn(move |_| {
                    let metrics = handle.handle().render();
                    let mut response = Response::new(metrics);
                    let content_type = HeaderValue::from_static("text/plain");
                    response.headers_mut().insert(CONTENT_TYPE, content_type);
                    async move { Ok::<_, Infallible>(response) }
                });

                tokio::spawn(async move {
                    let _ = jsonrpsee_server::serve(io, service).await.inspect_err(
                        |error| tracing::debug!(target: "ress", %error, "failed to serve request"),
                    );
                });
            }
        });
        Ok(())
    }
}
