use alloy_primitives::{map::HashMap, B256};
use futures::FutureExt;
use metrics::{Counter, Gauge, Histogram};
use reth_chainspec::ChainSpec;
use reth_metrics::Metrics;
use reth_node_ethereum::consensus::EthBeaconConsensus;
use reth_primitives::{SealedBlock, SealedHeader};
use reth_zk_ress_protocol::ExecutionProof;
use std::{
    collections::VecDeque,
    task::{Context, Poll},
    time::Duration,
};
use tracing::*;
use zk_ress_network::RessNetworkHandle;

/// Futures for fetching and validating blockchain data.
#[allow(missing_debug_implementations)]
pub mod futs;
use futs::*;

/// Struct for downloading chain data from the network.
#[allow(missing_debug_implementations)]
pub struct EngineDownloader<T> {
    network: RessNetworkHandle<T>,
    consensus: EthBeaconConsensus<ChainSpec>,
    retry_delay: Duration,

    inflight_full_block_requests: Vec<FetchFullBlockFuture<T>>,
    inflight_proof_requests: Vec<FetchProofFuture<T>>,
    inflight_finalized_block_requests: Vec<FetchFullBlockWithAncestorsFuture<T>>,
    outcomes: VecDeque<DownloadOutcome<T>>,

    metrics: EngineDownloaderMetrics,
}

impl<T: ExecutionProof> EngineDownloader<T> {
    /// Create new engine downloader.
    pub fn new(network: RessNetworkHandle<T>, consensus: EthBeaconConsensus<ChainSpec>) -> Self {
        Self {
            network,
            consensus,
            retry_delay: Duration::from_millis(50),
            inflight_full_block_requests: Vec::new(),
            inflight_proof_requests: Vec::new(),
            inflight_finalized_block_requests: Vec::new(),
            outcomes: VecDeque::new(),
            metrics: EngineDownloaderMetrics::default(),
        }
    }

    /// Download full block by block hash.
    pub fn download_full_block(&mut self, block_hash: B256) {
        if self.inflight_full_block_requests.iter().any(|req| req.block_hash() == block_hash) {
            return
        }

        debug!(target: "ress::engine::downloader", %block_hash, "Downloading full block");
        let fut = FetchFullBlockFuture::new(
            self.network.clone(),
            self.consensus.clone(),
            self.retry_delay,
            block_hash,
        );
        self.inflight_full_block_requests.push(fut);
        self.metrics.inc_total(RequestMetricTy::FullBlock);
        self.metrics.set_inflight(RequestMetricTy::FullBlock, self.inflight_proof_requests.len());
    }

    /// Download proof by block hash.
    pub fn download_proof(&mut self, block_hash: B256) {
        if self.inflight_proof_requests.iter().any(|req| req.block_hash() == block_hash) {
            return
        }

        debug!(target: "ress::engine::downloader", %block_hash, "Downloading proof");
        let fut = FetchProofFuture::new(self.network.clone(), self.retry_delay, block_hash);
        self.inflight_proof_requests.push(fut);
        self.metrics.inc_total(RequestMetricTy::Proof);
        self.metrics.set_inflight(RequestMetricTy::Proof, self.inflight_proof_requests.len());
    }

    /// Download finalized block with 256 ancestors.
    pub fn download_finalized_with_ancestors(&mut self, block_hash: B256) {
        if self.inflight_finalized_block_requests.iter().any(|req| req.block_hash() == block_hash) {
            return
        }

        debug!(target: "ress::engine::downloader", %block_hash, "Downloading finalized");
        let fut = FetchFullBlockWithAncestorsFuture::new(
            self.network.clone(),
            self.consensus.clone(),
            self.retry_delay,
            block_hash,
            256,
        );
        self.inflight_finalized_block_requests.push(fut);
        self.metrics.inc_total(RequestMetricTy::Finalized);
        self.metrics
            .set_inflight(RequestMetricTy::Finalized, self.inflight_finalized_block_requests.len());
    }

    /// Poll downloader.
    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<DownloadOutcome<T>> {
        if let Some(outcome) = self.outcomes.pop_front() {
            return Poll::Ready(outcome)
        }

        // advance all full block range requests
        for idx in (0..self.inflight_finalized_block_requests.len()).rev() {
            let mut request = self.inflight_finalized_block_requests.swap_remove(idx);
            if let Poll::Ready((block, ancestors)) = request.poll_unpin(cx) {
                let elapsed = request.elapsed();
                self.metrics.record_elapsed(RequestMetricTy::Finalized, elapsed);
                trace!(target: "ress::engine::downloader", block=?block.num_hash(), ancestors_len = ancestors.len(), ?elapsed, "Received finalized block");
                self.outcomes.push_back(DownloadOutcome::new(
                    DownloadData::FinalizedBlock(block, ancestors),
                    elapsed,
                ));
            } else {
                self.inflight_finalized_block_requests.push(request);
            }
        }
        self.metrics
            .set_inflight(RequestMetricTy::Finalized, self.inflight_finalized_block_requests.len());

        // advance all full block requests
        for idx in (0..self.inflight_full_block_requests.len()).rev() {
            let mut request = self.inflight_full_block_requests.swap_remove(idx);
            if let Poll::Ready(block) = request.poll_unpin(cx) {
                let elapsed = request.elapsed();
                self.metrics.record_elapsed(RequestMetricTy::FullBlock, elapsed);
                trace!(target: "ress::engine::downloader", block = ?block.num_hash(), ?elapsed, "Received single full block");
                self.outcomes
                    .push_back(DownloadOutcome::new(DownloadData::FullBlock(block), elapsed));
            } else {
                self.inflight_full_block_requests.push(request);
            }
        }
        self.metrics
            .set_inflight(RequestMetricTy::FullBlock, self.inflight_full_block_requests.len());

        // advance all proof requests
        for idx in (0..self.inflight_proof_requests.len()).rev() {
            let mut request = self.inflight_proof_requests.swap_remove(idx);
            if let Poll::Ready(proof) = request.poll_unpin(cx) {
                let elapsed = request.elapsed();
                self.metrics.record_elapsed(RequestMetricTy::Proof, elapsed);
                trace!(target: "ress::engine::downloader", block_hash = %request.block_hash(), ?elapsed, "Received proof");
                self.outcomes.push_back(DownloadOutcome::new(
                    DownloadData::Proof(request.block_hash(), proof),
                    elapsed,
                ));
            } else {
                self.inflight_proof_requests.push(request);
            }
        }
        self.metrics.set_inflight(RequestMetricTy::Proof, self.inflight_proof_requests.len());

        if let Some(outcome) = self.outcomes.pop_front() {
            return Poll::Ready(outcome)
        }

        Poll::Pending
    }
}

/// Download outcome.
#[derive(Debug)]
pub struct DownloadOutcome<T> {
    /// Downloaded data.
    pub data: DownloadData<T>,
    /// Time elapsed since download started.
    pub elapsed: Duration,
}

impl<T> DownloadOutcome<T> {
    /// Create new download outcome.
    pub fn new(data: DownloadData<T>, elapsed: Duration) -> Self {
        Self { data, elapsed }
    }
}

/// Download data.
#[derive(Debug)]
pub enum DownloadData<T> {
    /// Downloaded full block.
    FullBlock(SealedBlock),
    /// Downloaded execution proof.
    Proof(B256, T),
    /// Downloaded full block with ancestors.
    FinalizedBlock(SealedBlock, Vec<SealedHeader>),
}

#[derive(Default, Debug)]
struct EngineDownloaderMetrics {
    by_type: HashMap<RequestMetricTy, DownloadRequestTypeMetrics>,
}

impl EngineDownloaderMetrics {
    fn for_type(&mut self, ty: RequestMetricTy) -> &DownloadRequestTypeMetrics {
        self.by_type.entry(ty).or_insert_with(|| {
            DownloadRequestTypeMetrics::new_with_labels(&[("type", ty.to_string())])
        })
    }

    fn inc_total(&mut self, ty: RequestMetricTy) {
        self.for_type(ty).total.increment(1);
    }

    fn set_inflight(&mut self, ty: RequestMetricTy, count: usize) {
        self.for_type(ty).inflight.set(count as f64);
    }

    fn record_elapsed(&mut self, ty: RequestMetricTy, elapsed: Duration) {
        self.for_type(ty).elapsed.record(elapsed.as_secs_f64());
    }
}

#[derive(Metrics)]
#[metrics(scope = "engine.downloader")]
struct DownloadRequestTypeMetrics {
    /// The total number of requests.
    total: Counter,
    /// The number of inflight requests.
    inflight: Gauge,
    /// The number of seconds request took to complete.
    elapsed: Histogram,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, strum_macros::Display)]
#[strum(serialize_all = "snake_case")]
enum RequestMetricTy {
    FullBlock,
    Proof,
    Finalized,
}
