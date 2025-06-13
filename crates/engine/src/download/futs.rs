use alloy_primitives::B256;
use futures::FutureExt;
use reth_chainspec::ChainSpec;
use reth_consensus::{Consensus, HeaderValidator};
use reth_node_ethereum::consensus::EthBeaconConsensus;
use reth_primitives::{Block, BlockBody, Header, SealedBlock, SealedHeader};
use reth_ress_protocol::GetHeaders;
use reth_zk_ress_protocol::ExecutionProof;
use std::{
    future::Future,
    pin::Pin,
    task::{ready, Context, Poll},
    time::{Duration, Instant},
};
use tracing::*;
use zk_ress_network::{PeerRequestError, RessNetworkHandle};
use zk_ress_primitives::{TryFromNetworkProof, ZkRessPrimitives};

type DownloadFut<Ok> = Pin<Box<dyn Future<Output = Result<Ok, PeerRequestError>> + Send + Sync>>;

/// A future that downloads a full block from the network.
///
/// This will attempt to fetch both the header and body for the given block hash at the same time.
/// When both requests succeed, the future will yield the full block.
#[must_use = "futures do nothing unless polled"]
pub struct FetchFullBlockFuture<T> {
    network: RessNetworkHandle<T>,
    consensus: EthBeaconConsensus<ChainSpec>,
    retry_delay: Duration,
    block_hash: B256,
    started_at: Instant,
    pending_header_request: Option<DownloadFut<Option<Header>>>,
    pending_body_request: Option<DownloadFut<Option<BlockBody>>>,
    header: Option<SealedHeader>,
    body: Option<BlockBody>,
}

impl<T> FetchFullBlockFuture<T>
where
    T: Send + Sync + 'static,
{
    /// Create new fetch full block future.
    pub fn new(
        network: RessNetworkHandle<T>,
        consensus: EthBeaconConsensus<ChainSpec>,
        retry_delay: Duration,
        block_hash: B256,
    ) -> Self {
        let mut this = FetchFullBlockFuture {
            network,
            consensus,
            retry_delay,
            block_hash,
            started_at: Instant::now(),
            pending_header_request: None,
            pending_body_request: None,
            header: None,
            body: None,
        };
        this.pending_header_request = Some(this.header_request(Duration::default()));
        this.pending_body_request = Some(this.body_request(Duration::default()));
        this
    }

    /// Returns the hash of the block being requested.
    pub const fn block_hash(&self) -> B256 {
        self.block_hash
    }

    /// The duration elapsed since request was started.
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn header_request(&self, delay: Duration) -> DownloadFut<Option<Header>> {
        let network = self.network.clone();
        let hash = self.block_hash;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            let request = GetHeaders { start_hash: hash, limit: 1 };
            network.fetch_headers(request).await.map(|res| res.into_iter().next())
        })
    }

    fn body_request(&self, delay: Duration) -> DownloadFut<Option<BlockBody>> {
        let network = self.network.clone();
        let hash = self.block_hash;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            let request = Vec::from([hash]);
            network.fetch_block_bodies(request).await.map(|res| res.into_iter().next())
        })
    }

    fn on_header_response(&mut self, response: Result<Option<Header>, PeerRequestError>) {
        match response {
            Ok(Some(header)) => {
                let header = SealedHeader::seal_slow(header);
                if header.hash() == self.block_hash {
                    self.header = Some(header);
                } else {
                    trace!(target: "ress::engine::downloader", expected = %self.block_hash, received = %header.hash(), "Received wrong header");
                }
            }
            Ok(None) => {
                trace!(target: "ress::engine::downloader", block_hash = %self.block_hash, "No header received");
            }
            Err(error) => {
                trace!(target: "ress::engine::downloader", %error, %self.block_hash, "Header download failed");
            }
        };
    }

    fn on_body_response(&mut self, response: Result<Option<BlockBody>, PeerRequestError>) {
        match response {
            Ok(Some(body)) => {
                self.body = Some(body);
            }
            Ok(None) => {
                trace!(target: "ress::engine::downloader", block_hash = %self.block_hash, "No body received");
            }
            Err(error) => {
                trace!(target: "ress::engine::downloader", %error, %self.block_hash, "Body download failed");
            }
        }
    }
}

impl<T> Future for FetchFullBlockFuture<T>
where
    T: Send + Sync + 'static,
{
    type Output = SealedBlock;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            if let Some(fut) = &mut this.pending_header_request {
                if let Poll::Ready(response) = fut.poll_unpin(cx) {
                    this.pending_header_request.take();
                    this.on_header_response(response);
                    if this.header.is_none() {
                        this.pending_header_request = Some(this.header_request(this.retry_delay));
                        continue
                    }
                }
            }

            if let Some(fut) = &mut this.pending_body_request {
                if let Poll::Ready(response) = fut.poll_unpin(cx) {
                    this.pending_body_request.take();
                    this.on_body_response(response);
                    if this.body.is_none() {
                        this.pending_body_request = Some(this.body_request(this.retry_delay));
                        continue
                    }
                }
            }

            if this.header.is_some() && this.body.is_some() {
                let header = this.header.take().unwrap();
                let body = this.body.take().unwrap();

                // ensure the block is valid, else retry
                if let Err(error) = <EthBeaconConsensus<ChainSpec> as Consensus<Block>>::validate_body_against_header(&this.consensus, &body, &header) {
                    trace!(target: "ress::engine::downloader", %error, hash = %header.hash(), "Received wrong body");
                    this.header = Some(header);
                    this.pending_body_request = Some(this.body_request(this.retry_delay));
                    continue
                }

                return Poll::Ready(SealedBlock::from_sealed_parts(header, body))
            }

            return Poll::Pending
        }
    }
}

/// A future that downloads headers range.
#[must_use = "futures do nothing unless polled"]
pub struct FetchHeadersRangeFuture<T> {
    network: RessNetworkHandle<T>,
    consensus: EthBeaconConsensus<ChainSpec>,
    retry_delay: Duration,
    request: GetHeaders,
    started_at: Instant,
    pending: DownloadFut<Vec<Header>>,
}

impl<T> FetchHeadersRangeFuture<T>
where
    T: Send + Sync + 'static,
{
    /// Create new fetch headers range future.
    pub fn new(
        network: RessNetworkHandle<T>,
        consensus: EthBeaconConsensus<ChainSpec>,
        retry_delay: Duration,
        request: GetHeaders,
    ) -> Self {
        let network_ = network.clone();
        Self {
            network,
            consensus,
            retry_delay,
            request,
            started_at: Instant::now(),
            pending: Box::pin(async move { network_.fetch_headers(request).await }),
        }
    }

    /// Returns the get headers request.
    pub fn request(&self) -> GetHeaders {
        self.request
    }

    /// The duration elapsed since request was started.
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn request_headers(&self) -> DownloadFut<Vec<Header>> {
        let network = self.network.clone();
        let request = self.request;
        let delay = self.retry_delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            network.fetch_headers(request).await
        })
    }

    fn on_response(
        &mut self,
        response: Result<Vec<Header>, PeerRequestError>,
    ) -> Option<Vec<SealedHeader>> {
        let headers = match response {
            Ok(headers) => headers,
            Err(error) => {
                trace!(target: "ress::engine::downloader", %error, ?self.request, "Headers download failed");
                return None
            }
        };

        if headers.len() < self.request.limit as usize {
            trace!(target: "ress::engine::downloader", len = headers.len(), request = ?self.request, "Invalid headers response length");
            return None
        }

        let headers_falling = headers.into_iter().map(SealedHeader::seal_slow).collect::<Vec<_>>();
        if headers_falling[0].hash() != self.request.start_hash {
            trace!(target: "ress::engine::downloader", expected = %self.request.start_hash, received = %headers_falling[0].hash(), "Invalid start hash");
            return None
        }

        let headers_rising = headers_falling.iter().rev().cloned().collect::<Vec<_>>();
        // check if the downloaded headers are valid
        match self.consensus.validate_header_range(&headers_rising) {
            Ok(()) => Some(headers_falling),
            Err(error) => {
                trace!(target: "ress::engine::downloader", %error, ?self.request, "Received bad header response");
                None
            }
        }
    }
}

impl<T> Future for FetchHeadersRangeFuture<T>
where
    T: Send + Sync + 'static,
{
    type Output = Vec<SealedHeader>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            let response = ready!(this.pending.poll_unpin(cx));
            if let Some(headers) = this.on_response(response) {
                return Poll::Ready(headers)
            }
            this.pending = this.request_headers();
        }
    }
}

enum FullBlockWithAncestorsDownloadState<T> {
    FullBlock(FetchFullBlockFuture<T>),
    Ancestors(SealedBlock, FetchHeadersRangeFuture<T>),
}

/// A future that downloads full block and the headers of its ancestors.
#[must_use = "futures do nothing unless polled"]
pub struct FetchFullBlockWithAncestorsFuture<T> {
    block_hash: B256,
    ancestor_count: u64,
    state: FullBlockWithAncestorsDownloadState<T>,
    started_at: Instant,
}

impl<T> FetchFullBlockWithAncestorsFuture<T>
where
    T: Send + Sync + 'static,
{
    /// Create new fetch full block with ancestors future.
    pub fn new(
        network: RessNetworkHandle<T>,
        consensus: EthBeaconConsensus<ChainSpec>,
        retry_delay: Duration,
        block_hash: B256,
        ancestor_count: u64,
    ) -> Self {
        let state = FullBlockWithAncestorsDownloadState::FullBlock(FetchFullBlockFuture::new(
            network,
            consensus,
            retry_delay,
            block_hash,
        ));
        Self { block_hash, ancestor_count, state, started_at: Instant::now() }
    }

    /// Returns the hash of the block being requested.
    pub const fn block_hash(&self) -> B256 {
        self.block_hash
    }

    /// The duration elapsed since request was started.
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }
}

impl<T> Future for FetchFullBlockWithAncestorsFuture<T>
where
    T: Send + Sync + 'static,
{
    type Output = (SealedBlock, Vec<SealedHeader>);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                FullBlockWithAncestorsDownloadState::FullBlock(fut) => {
                    let block = ready!(fut.poll_unpin(cx));
                    let ancestors_fut = FetchHeadersRangeFuture::new(
                        fut.network.clone(),
                        fut.consensus.clone(),
                        fut.retry_delay,
                        GetHeaders { start_hash: block.parent_hash, limit: this.ancestor_count },
                    );
                    this.state =
                        FullBlockWithAncestorsDownloadState::Ancestors(block, ancestors_fut);
                }
                FullBlockWithAncestorsDownloadState::Ancestors(block, fut) => {
                    let ancestors = ready!(fut.poll_unpin(cx));
                    return Poll::Ready((std::mem::take(block), ancestors))
                }
            }
        }
    }
}

enum FullBlockRangeDownloadState<T> {
    Headers { fut: FetchHeadersRangeFuture<T> },
    Bodies(FullBlockRangeBodiesDownloadState),
}

struct FullBlockRangeBodiesDownloadState {
    headers: Vec<SealedHeader>,
    fut: DownloadFut<Vec<BlockBody>>,
    bodies: Vec<BlockBody>,
}

impl FullBlockRangeBodiesDownloadState {
    fn missing(&self) -> impl Iterator<Item = B256> + '_ {
        self.headers.iter().skip(self.bodies.len()).map(|h| h.hash())
    }

    fn take_blocks(&mut self) -> impl Iterator<Item = SealedBlock> {
        std::mem::take(&mut self.headers)
            .into_iter()
            .zip(std::mem::take(&mut self.bodies))
            .map(|(header, body)| SealedBlock::from_sealed_parts(header, body))
    }
}

/// A future that downloads full block range.
#[must_use = "futures do nothing unless polled"]
pub struct FetchFullBlockRangeFuture<T> {
    network: RessNetworkHandle<T>,
    consensus: EthBeaconConsensus<ChainSpec>,
    retry_delay: Duration,
    request: GetHeaders,
    started_at: Instant,
    state: FullBlockRangeDownloadState<T>,
}

impl<T: ExecutionProof> FetchFullBlockRangeFuture<T> {
    /// Create new fetch full block range future.
    pub fn new(
        network: RessNetworkHandle<T>,
        consensus: EthBeaconConsensus<ChainSpec>,
        retry_delay: Duration,
        request: GetHeaders,
    ) -> Self {
        let fut =
            FetchHeadersRangeFuture::new(network.clone(), consensus.clone(), retry_delay, request);
        Self {
            network,
            consensus,
            retry_delay,
            request,
            started_at: fut.started_at,
            state: FullBlockRangeDownloadState::Headers { fut },
        }
    }

    /// Returns the get headers request.
    pub fn request(&self) -> GetHeaders {
        self.request
    }

    /// The duration elapsed since request was started.
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn request_bodies(
        network: RessNetworkHandle<T>,
        request: impl IntoIterator<Item = B256>,
        delay: Duration,
    ) -> DownloadFut<Vec<BlockBody>> {
        let request = request.into_iter().collect();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            network.fetch_block_bodies(request).await
        })
    }
}

impl<T: ExecutionProof> Future for FetchFullBlockRangeFuture<T> {
    type Output = Vec<SealedBlock>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match &mut this.state {
                FullBlockRangeDownloadState::Headers { fut } => {
                    let headers = ready!(fut.poll_unpin(cx));
                    let fut = Self::request_bodies(
                        this.network.clone(),
                        headers.iter().map(|h| h.hash()),
                        Default::default(),
                    );
                    this.state =
                        FullBlockRangeDownloadState::Bodies(FullBlockRangeBodiesDownloadState {
                            headers,
                            fut,
                            bodies: Vec::new(),
                        });
                }
                FullBlockRangeDownloadState::Bodies(state) => {
                    let response = ready!(state.fut.poll_unpin(cx));
                    let pending_bodies = match response {
                        Ok(pending) => {
                            if pending.is_empty() {
                                trace!(target: "ress::engine::downloader", request = ?this.request, "Empty bodies response");
                                state.fut = Self::request_bodies(
                                    this.network.clone(),
                                    state.missing(),
                                    this.retry_delay,
                                );
                                continue
                            }
                            pending
                        }
                        Err(error) => {
                            trace!(target: "ress::engine::downloader", %error, ?this.request, "Bodies download failed");
                            state.fut = Self::request_bodies(
                                this.network.clone(),
                                state.missing(),
                                this.retry_delay,
                            );
                            continue
                        }
                    };

                    let mut pending_bodies = pending_bodies.into_iter();
                    for header in &state.headers[state.bodies.len()..] {
                        if let Some(body) = pending_bodies.next() {
                            if let Err(error) = <EthBeaconConsensus<ChainSpec> as Consensus<
                                Block,
                            >>::validate_body_against_header(
                                &this.consensus, &body, header
                            ) {
                                trace!(target: "ress::engine::downloader", %error, ?this.request, "Invalid body response");
                                state.fut = Self::request_bodies(
                                    this.network.clone(),
                                    state.missing(),
                                    this.retry_delay,
                                );
                                continue
                            }

                            state.bodies.push(body);
                        }
                    }

                    let remaining_hashes = state.missing().collect::<Vec<_>>();
                    if !remaining_hashes.is_empty() {
                        state.fut = Self::request_bodies(
                            this.network.clone(),
                            remaining_hashes,
                            Default::default(),
                        );
                        continue
                    }

                    return Poll::Ready(state.take_blocks().collect())
                }
            }
        }
    }
}

/// A future that downloads a proof from the network.
#[must_use = "futures do nothing unless polled"]
pub struct FetchProofFuture<P: ZkRessPrimitives> {
    network: RessNetworkHandle<P::NetworkProof>,
    block_hash: B256,
    retry_delay: Duration,
    started_at: Instant,
    pending: DownloadFut<P::NetworkProof>,
}

impl<P: ZkRessPrimitives> FetchProofFuture<P> {
    /// Create new fetch proof future.
    pub fn new(
        network: RessNetworkHandle<P::NetworkProof>,
        retry_delay: Duration,
        block_hash: B256,
    ) -> Self {
        let network_ = network.clone();
        Self {
            network,
            retry_delay,
            block_hash,
            started_at: Instant::now(),
            pending: Box::pin(async move { network_.fetch_proof(block_hash).await }),
        }
    }

    /// Returns the hash of the block the proof is being requested for.
    pub fn block_hash(&self) -> B256 {
        self.block_hash
    }

    /// The duration elapsed since request was started.
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn proof_request(&self) -> DownloadFut<P::NetworkProof> {
        let network = self.network.clone();
        let hash = self.block_hash;
        let delay = self.retry_delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            network.fetch_proof(hash).await
        })
    }
}

impl<P: ZkRessPrimitives> Future for FetchProofFuture<P> {
    type Output = P::Proof;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match ready!(this.pending.poll_unpin(cx)) {
                Ok(proof) => {
                    match <P::Proof as TryFromNetworkProof<P::NetworkProof>>::try_from(proof) {
                        Ok(proof) => {
                            if proof.is_empty() {
                                trace!(target: "ress::engine::downloader", block_hash = %this.block_hash, "Received empty proof");
                            } else {
                                return Poll::Ready(proof)
                            }
                        }
                        Err(error) => {
                            // TODO: log error
                            trace!(target: "ress::engine::downloader", block_hash = %this.block_hash, ?error, "Could not convert network proof");
                        }
                    }
                }
                Err(error) => {
                    trace!(target: "ress::engine::downloader", %error, %this.block_hash, "Proof download failed");
                }
            };
            this.pending = this.proof_request();
        }
    }
}
