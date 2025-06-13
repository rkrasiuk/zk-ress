use futures::StreamExt;
use reth_network_api::PeerId;
use reth_zk_ress_protocol::{ProtocolEvent, ZkRessPeerRequest};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, trace};

/// Peer connection handle.
#[derive(Debug)]
struct ConnectionHandle {
    peer_id: PeerId,
    to_connection: mpsc::UnboundedSender<ZkRessPeerRequest>,
}

/// Network manager for forwarding requests to peer connections.
#[derive(Debug)]
pub struct RessNetworkManager {
    protocol_events: UnboundedReceiverStream<ProtocolEvent>,
    peer_requests: UnboundedReceiverStream<ZkRessPeerRequest>,
    connections: VecDeque<ConnectionHandle>,
    pending_requests: VecDeque<ZkRessPeerRequest>,
}

impl RessNetworkManager {
    /// Create new network manager.
    pub fn new(
        protocol_events: UnboundedReceiverStream<ProtocolEvent>,
        peer_requests: UnboundedReceiverStream<ZkRessPeerRequest>,
    ) -> Self {
        Self {
            protocol_events,
            peer_requests,
            connections: VecDeque::new(),
            pending_requests: VecDeque::new(),
        }
    }

    fn on_peer_request(&mut self, mut request: ZkRessPeerRequest) {
        // Rotate connections for peer requests
        while let Some(connection) = self.connections.pop_front() {
            trace!(target: "ress::net", peer_id = %connection.peer_id, ?request, "Sending request to peer");
            match connection.to_connection.send(request) {
                Ok(()) => {
                    self.connections.push_back(connection);
                    return
                }
                Err(mpsc::error::SendError(request_)) => {
                    request = request_;
                    trace!(target: "ress::net", peer_id = %connection.peer_id, ?request, "Failed to send request, connection closed");
                }
            }
        }
        trace!(target: "ress::net", ?request, "No connections are available");
        self.pending_requests.push_back(request);
    }
}

impl Future for RessNetworkManager {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            if !this.connections.is_empty() && !this.pending_requests.is_empty() {
                let request = this.pending_requests.pop_front().unwrap();
                this.on_peer_request(request);
                continue
            }

            if let Poll::Ready(Some(ProtocolEvent::Established {
                direction,
                peer_id,
                to_connection,
            })) = this.protocol_events.poll_next_unpin(cx)
            {
                debug!(target: "ress::net", %peer_id, %direction, "Peer connection established");
                this.connections.push_back(ConnectionHandle { peer_id, to_connection });
                continue
            }

            if let Poll::Ready(Some(request)) = this.peer_requests.poll_next_unpin(cx) {
                this.on_peer_request(request);
                continue
            }

            return Poll::Pending
        }
    }
}
