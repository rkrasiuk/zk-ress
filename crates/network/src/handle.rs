use alloy_primitives::{Bytes, B256};
use reth_network::NetworkHandle;
use reth_primitives::{BlockBody, Header};
use reth_ress_protocol::GetHeaders;
use reth_zk_ress_protocol::ZkRessPeerRequest;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tracing::trace;

/// Ress networking handle.
#[derive(Clone, Debug)]
pub struct RessNetworkHandle {
    /// Handle for interacting with the network.
    network_handle: NetworkHandle,
    /// Sender for forwarding peer requests.
    peer_requests_sender: mpsc::UnboundedSender<ZkRessPeerRequest>,
}

impl RessNetworkHandle {
    /// Create new network handle from reth's handle and peer connection.
    pub fn new(
        network_handle: NetworkHandle,
        peer_requests_sender: mpsc::UnboundedSender<ZkRessPeerRequest>,
    ) -> Self {
        Self { network_handle, peer_requests_sender }
    }

    /// Return reference to reth's network handle.
    pub fn inner(&self) -> &NetworkHandle {
        &self.network_handle
    }

    fn send_request(&self, request: ZkRessPeerRequest) -> Result<(), PeerRequestError> {
        self.peer_requests_sender.send(request).map_err(|_| PeerRequestError::ConnectionClosed)
    }
}

impl RessNetworkHandle {
    /// Get block headers.
    pub async fn fetch_headers(
        &self,
        request: GetHeaders,
    ) -> Result<Vec<Header>, PeerRequestError> {
        trace!(target: "ress::net", ?request, "requesting header");
        let (tx, rx) = oneshot::channel();
        self.send_request(ZkRessPeerRequest::GetHeaders { request, tx })?;
        let response = rx.await.map_err(|_| PeerRequestError::RequestDropped)?;
        trace!(target: "ress::net", ?request, "headers received");
        Ok(response)
    }

    /// Get block bodies.
    pub async fn fetch_block_bodies(
        &self,
        request: Vec<B256>,
    ) -> Result<Vec<BlockBody>, PeerRequestError> {
        trace!(target: "ress::net", ?request, "requesting block bodies");
        let (tx, rx) = oneshot::channel();
        self.send_request(ZkRessPeerRequest::GetBlockBodies { request: request.clone(), tx })?;
        let response = rx.await.map_err(|_| PeerRequestError::RequestDropped)?;
        trace!(target: "ress::net", ?request, "block bodies received");
        Ok(response)
    }

    /// Get execution proof from block hash
    pub async fn fetch_proof(&self, block_hash: B256) -> Result<Bytes, PeerRequestError> {
        trace!(target: "ress::net", %block_hash, "requesting proof");
        let (tx, rx) = oneshot::channel();
        self.send_request(ZkRessPeerRequest::GetProof { block_hash, tx })?;
        let response = rx.await.map_err(|_| PeerRequestError::RequestDropped)?;
        trace!(target: "ress::net", %block_hash, "proof received");
        Ok(response)
    }
}

/// Peer request errors.
#[derive(Debug, Error)]
pub enum PeerRequestError {
    /// Request dropped.
    #[error("Peer request dropped")]
    RequestDropped,

    /// Connection closed.
    #[error("Peer connection was closed")]
    ConnectionClosed,
}
