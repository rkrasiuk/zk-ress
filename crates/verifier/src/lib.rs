//! Block verifier for `zk-ress` stateless client.

use alloy_consensus::{BlockHeader, Header};
use alloy_primitives::{keccak256, map::B256Map, Bytes, B256};
use alloy_rlp::Decodable;
use rayon::prelude::*;
use reth_chainspec::ChainSpec;
use reth_consensus::{Consensus as _, FullConsensus, HeaderValidator as _};
use reth_errors::{BlockExecutionError, ConsensusError, ProviderError};
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_primitives::{Block, EthPrimitives, GotExpected, RecoveredBlock, SealedHeader};
use reth_ress_protocol::ExecutionStateWitness;
use reth_revm::state::Bytecode;
use reth_trie::{HashedPostState, KeccakKeyHasher};
use reth_trie_sparse::{blinded::DefaultBlindedProviderFactory, SparseStateTrie};
use std::{collections::BTreeMap, time::Instant};
use tracing::*;
use zk_ress_evm::BlockExecutor;
use zk_ress_provider::ZkRessProvider;

mod root;
pub use root::calculate_state_root;

/// Implementation of block verification.
///
/// Given the proof, it must verify full validity of the block according to Ethereum consensus.
pub trait BlockVerifier: Unpin {
    /// Verify that the block is valid.
    fn verify(
        &self,
        block: RecoveredBlock<Block>,
        parent: SealedHeader,
        proof: Bytes,
    ) -> Result<(), VerifierError>;
}

/// All error variants possible when verifying a block.
#[derive(Debug, thiserror::Error)]
pub enum VerifierError {
    /// Block violated consensus rules.
    #[error(transparent)]
    Consensus(#[from] ConsensusError),
    /// Block execution failed.
    #[error(transparent)]
    Execution(#[from] BlockExecutionError),
    /// Provider error.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// Other errors.
    #[error(transparent)]
    Other(#[from] Box<dyn core::error::Error + Send + Sync + 'static>),
}

/// Block verifier to validate the block by fully validating and
/// re-executing it using execution witness.
#[derive(Debug)]
pub struct ExecutionWitnessVerifier {
    provider: ZkRessProvider,
    consensus: EthBeaconConsensus<ChainSpec>,
}

impl ExecutionWitnessVerifier {
    /// Create new execution witness block verifier.
    pub fn new(
        provider: ZkRessProvider,
        consensus: EthBeaconConsensus<ChainSpec>,
    ) -> Self {
        Self { provider, consensus }
    }
}

impl BlockVerifier for ExecutionWitnessVerifier {
    fn verify(
        &self,
        block: RecoveredBlock<Block>,
        parent: SealedHeader,
        proof: Bytes,
    ) -> Result<(), VerifierError> {
        // Decode ExecutionStateWitness from Bytes
        let proof = ExecutionStateWitness::decode(&mut &proof[..])
            .map_err(|e| VerifierError::Other(Box::new(e)))?;
        let block_num_hash = block.num_hash();

        // let chain_spec = self.provider.chain_spec();

        // let evm_config = EthEvmConfig::new(chain_spec.clone());

        // // TODO: Merge these two `ExecutionWitness` types
        // let execution_witness = reth_stateless::ExecutionWitness {
        //     state: proof.state,
        //     codes: proof.bytecodes,
        //     // Keys are not used at the moment
        //     keys: Vec::new(),
        //     headers: proof.headers,
        // };
        // reth_stateless::validation::stateless_validation(
        //     block.clone_block(),
        //     execution_witness,
        //     chain_spec,
        //     evm_config,
        // )
        // .map_err(|err| VerifierError::Other(Box::new(err)))?;

        // return Ok(());

        // ===================== Pre Execution Validation =====================
        self.consensus.validate_header(block.sealed_header()).inspect_err(|error| {
            error!(target: "ress::engine", %error, "Failed to validate header");
        })?;

        self.consensus.validate_block_pre_execution(&block).inspect_err(|error| {
            error!(target: "ress::engine", %error, "Failed to validate block");
        })?;

        self.consensus.validate_header_against_parent(block.sealed_header(), &parent).inspect_err(
            |error| {
                error!(target: "ress::engine", %error, "Failed to validate header against parent");
            },
        )?;

        // ===================== Witness =====================
        let mut trie = SparseStateTrie::new(DefaultBlindedProviderFactory);
        let mut state_witness = B256Map::default();
        for encoded in proof.state {
            state_witness.insert(keccak256(&encoded), encoded);
        }

        trie.reveal_witness(parent.state_root, &state_witness)
            .map_err(|error| ProviderError::TrieWitnessError(error.to_string()))?;

        let mut bytecodes = B256Map::default();
        for bytes in proof.bytecodes {
            let bytecode = Bytecode::new_raw(bytes);
            let code_hash = bytecode.hash_slow();
            bytecodes.insert(code_hash, bytecode);
        }

        let ancestor_headers: BTreeMap<u64, B256> = proof
            .headers
            .iter()
            .map(|serialized_header| {
                let bytes = serialized_header.as_ref();
                let header = Header::decode(&mut &bytes[..]).unwrap();
                (header.number, header.hash_slow())
            })
            .collect();

        // ===================== Execution =====================
        let start_time = Instant::now();
        let block_executor = BlockExecutor::new(
            self.provider.chain_spec(),
            block.parent_num_hash(),
            &trie,
            &bytecodes,
            ancestor_headers,
        );
        let output = block_executor.execute(&block)?;
        debug!(target: "zk_ress::engine", block = ?block_num_hash, elapsed = ?start_time.elapsed(), "Executed new payload");

        // ===================== Post Execution Validation =====================
        <EthBeaconConsensus<ChainSpec> as FullConsensus<EthPrimitives>>::validate_block_post_execution(
            &self.consensus,
            &block,
            &output.result
        )?;

        // ===================== State Root =====================
        let hashed_state =
            HashedPostState::from_bundle_state::<KeccakKeyHasher>(output.state.state.par_iter());
        let state_root = calculate_state_root(&mut trie, hashed_state)
            .map_err(|error| ProviderError::TrieWitnessError(error.to_string()))?;
        if state_root != block.state_root {
            return Err(ConsensusError::BodyStateRootDiff(
                GotExpected { got: state_root, expected: block.state_root }.into(),
            )
            .into());
        }

        Ok(())
    }
}
