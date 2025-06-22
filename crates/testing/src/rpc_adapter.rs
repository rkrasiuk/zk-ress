use alloy_primitives::B256;
use alloy_provider::{Provider, RootProvider, WsConnect};
use alloy_rlp::{BytesMut, Encodable};
use alloy_rpc_client::ClientBuilder;
use alloy_rpc_types_debug::ExecutionWitness;
use alloy_rpc_types_eth::{Block, BlockId, BlockNumberOrTag, BlockTransactionsKind};
use futures::{stream::FuturesOrdered, StreamExt};
use reth_network::eth_requests::{MAX_BODIES_SERVE, MAX_HEADERS_SERVE, SOFT_RESPONSE_LIMIT};
use reth_primitives::{BlockBody, Header, TransactionSigned};
use reth_ress_protocol::GetHeaders;
use reth_zk_ress_protocol::ZkRessPeerRequest;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::*;
use tungstenite::protocol::WebSocketConfig;

/// RPC adapter that can substitute `RessNetworkManager`.
#[derive(Clone, Debug)]
pub struct RpcNetworkAdapter {
    provider: RootProvider,
}

impl RpcNetworkAdapter {
    /// Create new RPC adapter.
    pub async fn new(url: &str) -> eyre::Result<Self> {
        let client = ClientBuilder::default()
            .ws(WsConnect::new(url).with_config(
                WebSocketConfig::default()
                    .max_message_size(Some(128 << 20))
                    .max_frame_size(Some(64 << 20)),
            ))
            .await?;
        Ok(Self { provider: RootProvider::new(client) })
    }

    async fn block(
        &self,
        block_id: BlockId,
        transactions_kind: BlockTransactionsKind,
    ) -> Option<Block> {
        let result =
            self.provider.get_block(block_id).kind(transactions_kind).await.inspect_err(
                |error| {
                    debug!(target: "ress::rpc_adapter", %block_id, ?error, "Failed to request block from provider");
                },
            );
        result.ok().flatten()
    }

    async fn on_headers_request(&self, request: GetHeaders) -> Vec<Header> {
        let Some(start_block) =
            self.block(request.start_hash.into(), BlockTransactionsKind::Hashes).await
        else {
            return Vec::new();
        };

        let header = start_block.header.into_consensus();
        let start_block_number = header.number;
        let mut total_bytes = header.length();

        let mut headers = Vec::from([header]);
        if request.limit <= 1 {
            return headers;
        }

        let end_block_number = start_block_number
            .saturating_sub(std::cmp::min(request.limit - 1, MAX_HEADERS_SERVE as u64));

        let mut futs = FuturesOrdered::new();
        debug!(target: "ress::rpc_adapter", range = ?end_block_number..start_block_number, "Downloading headers for range");
        for block_number in end_block_number..start_block_number {
            let provider = self.clone();
            futs.push_front(Box::pin(async move {
                provider.block(block_number.into(), BlockTransactionsKind::Hashes).await
            }));
        }

        while let Some(block) = futs.next().await.flatten() {
            trace!(target: "ress::rpc_adapter", number = block.header.number, hash = %block.header.hash, "Downloaded block for header");
            let header = block.header.into_consensus();
            total_bytes += header.length();
            headers.push(header);
            if total_bytes > SOFT_RESPONSE_LIMIT {
                break
            }
        }

        headers
    }

    async fn on_block_bodies_request(&self, request: Vec<B256>) -> Vec<BlockBody> {
        let mut futs = FuturesOrdered::new();
        for block_hash in request {
            let provider = self.clone();
            futs.push_back(Box::pin(async move {
                provider.block(block_hash.into(), BlockTransactionsKind::Full).await
            }));
        }

        let mut total_bytes = 0;
        let mut bodies = Vec::new();
        while let Some(block) = futs.next().await.flatten() {
            trace!(target: "ress::rpc_adapter", number = block.header.number, hash = %block.header.hash, "Downloaded block for body");
            let block = block.map_transactions(|tx| TransactionSigned::from(tx.inner.into_inner()));
            let body = BlockBody {
                transactions: block.transactions.into_transactions().collect(),
                withdrawals: block.withdrawals.map(|w| w.into_inner().into()),
                ommers: Default::default(),
            };
            total_bytes += body.length();
            bodies.push(body);
            if bodies.len() >= MAX_BODIES_SERVE || total_bytes > SOFT_RESPONSE_LIMIT {
                break
            }
        }

        bodies
    }

    /// Run RPC network adapter to respond to peer requests.
    pub async fn run(self, mut request_stream: UnboundedReceiverStream<ZkRessPeerRequest>) {
        while let Some(request) = request_stream.next().await {
            match request {
                ZkRessPeerRequest::GetHeaders { request, tx } => {
                    let provider = self.clone();
                    tokio::spawn(async move {
                        let headers = provider.on_headers_request(request).await;
                        if tx.send(headers).is_err() {
                            debug!(target: "ress::rpc_adapter", ?request, "Failed to send header");
                        }
                    });
                }
                ZkRessPeerRequest::GetBlockBodies { request, tx } => {
                    let provider = self.clone();
                    tokio::spawn(async move {
                        let bodies = provider.on_block_bodies_request(request.clone()).await;
                        if tx.send(bodies).is_err() {
                            debug!(target: "ress::rpc_adapter", ?request, "Failed to send block body");
                        }
                    });
                }
                ZkRessPeerRequest::GetProof { block_hash, tx } => {
                    let provider = self.clone();
                    tokio::spawn(async move {
                        let maybe_block =
                            provider.block(block_hash.into(), BlockTransactionsKind::Hashes).await;
                        let maybe_witness = if let Some(block) = maybe_block {
                            let tag: BlockNumberOrTag = block.header.number.into();
                            let maybe_witness =  provider.provider
                                .client()
                                .request::<_, ExecutionWitness>("debug_executionWitness", [tag])
                                .await
                                .map_err(|error| {
                                    debug!(target: "ress::rpc_adapter", %block_hash, %error, "Failed to request witness from provider");
                                })
                                .ok();
                            maybe_witness.map(|witness| {
                                let mut encoded = BytesMut::default();
                                reth_ress_protocol::ExecutionStateWitness {
                                    state: witness.state,
                                    bytecodes: witness.codes,
                                    headers: witness.headers,
                                }
                                .encode(&mut encoded);
                                encoded.freeze().into()
                            })
                        } else {
                            None
                        };

                        if tx.send(maybe_witness.unwrap_or_default()).is_err() {
                            debug!(target: "ress::rpc_adapter", %block_hash, "Failed to send witness");
                        }
                    });
                }
            }
        }
    }
}
