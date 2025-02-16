use alloy_primitives::B256;
use futures::FutureExt;
use ress_network::RessNetworkHandle;
use ress_primitives::witness::ExecutionWitness;
use ress_protocol::GetHeaders;
use reth_chainspec::ChainSpec;
use reth_node_ethereum::consensus::EthBeaconConsensus;
use reth_primitives::{Bytecode, SealedBlock, SealedHeader};
use std::{
    collections::VecDeque,
    task::{Context, Poll},
    time::Duration,
};
use tracing::*;

/// Futures for fetching and validating blockchain data.
#[allow(missing_debug_implementations)]
pub mod futs;
use futs::*;

/// Struct for downloading chain data from the network.
#[allow(missing_debug_implementations)]
pub struct EngineDownloader {
    network: RessNetworkHandle,
    consensus: EthBeaconConsensus<ChainSpec>,
    retry_delay: Duration,

    inflight_headers_range_requests: Vec<FetchHeadersRangeFuture>,
    inflight_full_blocks_range_requests: Vec<FetchFullBlockRangeFuture>,
    inflight_full_block_requests: Vec<FetchFullBlockFuture>,
    inflight_bytecode_requests: Vec<FetchBytecodeFuture>,
    inflight_witness_requests: Vec<FetchWitnessFuture>,
    outcomes: VecDeque<DownloadOutcome>,
}

impl EngineDownloader {
    /// Create new engine downloader.
    pub fn new(network: RessNetworkHandle, consensus: EthBeaconConsensus<ChainSpec>) -> Self {
        Self {
            network,
            consensus,
            retry_delay: Duration::from_millis(50),
            inflight_headers_range_requests: Vec::new(),
            inflight_full_blocks_range_requests: Vec::new(),
            inflight_full_block_requests: Vec::new(),
            inflight_witness_requests: Vec::new(),
            inflight_bytecode_requests: Vec::new(),
            outcomes: VecDeque::new(),
        }
    }

    /// Download headers range starting from start hash.
    pub fn download_headers_range(&mut self, start_hash: B256, count: u64) {
        let request = GetHeaders { start_hash, limit: count };
        if self.inflight_headers_range_requests.iter().any(|req| req.request() == request) {
            return
        }

        trace!(target: "ress::engine::downloader", %start_hash, count, "Downloading headers range");
        let fut = FetchHeadersRangeFuture::new(
            self.network.clone(),
            self.consensus.clone(),
            self.retry_delay,
            request,
        );
        self.inflight_headers_range_requests.push(fut);
    }

    /// Download full blocks range starting from start hash.
    pub fn download_full_blocks_range(&mut self, start_hash: B256, count: u64) {
        let request = GetHeaders { start_hash, limit: count };
        if self.inflight_full_blocks_range_requests.iter().any(|req| req.request() == request) {
            return
        }

        trace!(target: "ress::engine::downloader", %start_hash, count, "Downloading full block range");
        let fut = FetchFullBlockRangeFuture::new(
            self.network.clone(),
            self.consensus.clone(),
            self.retry_delay,
            request,
        );
        self.inflight_full_blocks_range_requests.push(fut);
    }

    /// Download full block by block hash.
    pub fn download_full_block(&mut self, block_hash: B256) {
        if self.inflight_full_block_requests.iter().any(|req| req.block_hash() == block_hash) {
            return
        }

        trace!(target: "ress::engine::downloader", %block_hash, "Downloading full block");
        let fut = FetchFullBlockFuture::new(
            self.network.clone(),
            self.consensus.clone(),
            self.retry_delay,
            block_hash,
        );
        self.inflight_full_block_requests.push(fut);
    }

    /// Download bytecode by code hash.
    pub fn download_bytecode(&mut self, code_hash: B256) {
        if self.inflight_bytecode_requests.iter().any(|req| req.code_hash() == code_hash) {
            return
        }

        trace!(target: "ress::engine::downloader", %code_hash, "Downloading bytecode");
        let fut = FetchBytecodeFuture::new(self.network.clone(), self.retry_delay, code_hash);
        self.inflight_bytecode_requests.push(fut);
    }

    /// Download witness by block hash.
    pub fn download_witness(&mut self, block_hash: B256) {
        if self.inflight_witness_requests.iter().any(|req| req.block_hash() == block_hash) {
            return
        }

        trace!(target: "ress::engine::downloader", %block_hash, "Downloading witness");
        let fut = FetchWitnessFuture::new(self.network.clone(), self.retry_delay, block_hash);
        self.inflight_witness_requests.push(fut);
    }

    /// Poll downloader.
    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<DownloadOutcome> {
        if let Some(outcome) = self.outcomes.pop_front() {
            return Poll::Ready(outcome)
        }

        // advance all headers range requests
        for idx in (0..self.inflight_headers_range_requests.len()).rev() {
            let mut request = self.inflight_headers_range_requests.swap_remove(idx);
            if let Poll::Ready(headers) = request.poll_unpin(cx) {
                trace!(target: "ress::engine::downloader", request = ?request.request(), "Received headers range");
                self.outcomes.push_back(DownloadOutcome::new(
                    DownloadData::HeadersRange(headers),
                    request.elapsed(),
                ));
            } else {
                self.inflight_headers_range_requests.push(request);
            }
        }

        // advance all full block range requests
        for idx in (0..self.inflight_full_blocks_range_requests.len()).rev() {
            let mut request = self.inflight_full_blocks_range_requests.swap_remove(idx);
            if let Poll::Ready(blocks) = request.poll_unpin(cx) {
                trace!(target: "ress::engine::downloader", request = ?request.request(), "Received full block range");
                self.outcomes.push_back(DownloadOutcome::new(
                    DownloadData::FullBlocksRange(blocks),
                    request.elapsed(),
                ));
            } else {
                self.inflight_full_blocks_range_requests.push(request);
            }
        }

        // advance all full block requests
        for idx in (0..self.inflight_full_block_requests.len()).rev() {
            let mut request = self.inflight_full_block_requests.swap_remove(idx);
            if let Poll::Ready(block) = request.poll_unpin(cx) {
                trace!(target: "ress::engine::downloader", block=?block.num_hash(), "Received single full block");
                self.outcomes.push_back(DownloadOutcome::new(
                    DownloadData::FullBlock(block),
                    request.elapsed(),
                ));
            } else {
                self.inflight_full_block_requests.push(request);
            }
        }

        // advance all witness requests
        for idx in (0..self.inflight_witness_requests.len()).rev() {
            let mut request = self.inflight_witness_requests.swap_remove(idx);
            if let Poll::Ready(witness) = request.poll_unpin(cx) {
                trace!(target: "ress::engine::downloader", block_hash = %request.block_hash(), "Received witness");
                self.outcomes.push_back(DownloadOutcome::new(
                    DownloadData::Witness(request.block_hash(), witness),
                    request.elapsed(),
                ));
            } else {
                self.inflight_witness_requests.push(request);
            }
        }

        // advance all bytecode requests
        for idx in (0..self.inflight_bytecode_requests.len()).rev() {
            let mut request = self.inflight_bytecode_requests.swap_remove(idx);
            if let Poll::Ready(bytecode) = request.poll_unpin(cx) {
                trace!(target: "ress::engine::downloader", code_hash = %request.code_hash(), "Received bytecode");
                self.outcomes.push_back(DownloadOutcome::new(
                    DownloadData::Bytecode(request.code_hash(), bytecode),
                    request.elapsed(),
                ));
            } else {
                self.inflight_bytecode_requests.push(request);
            }
        }

        if let Some(outcome) = self.outcomes.pop_front() {
            return Poll::Ready(outcome)
        }

        Poll::Pending
    }
}

/// Download outcome.
#[derive(Debug)]
pub struct DownloadOutcome {
    /// Downloaded data.
    pub data: DownloadData,
    /// Time elapsed since download started.
    pub elapsed: Duration,
}

impl DownloadOutcome {
    /// Create new download outcome.
    pub fn new(data: DownloadData, elapsed: Duration) -> Self {
        Self { data, elapsed }
    }
}

/// Download data.
#[derive(Debug)]
pub enum DownloadData {
    /// Downloaded headers range.
    HeadersRange(Vec<SealedHeader>),
    /// Downloaded full blocks range.
    FullBlocksRange(Vec<SealedBlock>),
    /// Downloaded full block.
    FullBlock(SealedBlock),
    /// Downloaded bytecode.
    Bytecode(B256, Bytecode),
    /// Downloaded execution witness.
    Witness(B256, ExecutionWitness),
}
