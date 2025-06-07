//! Block verifier for `zk-ress` stateless client.

use alloy_primitives::{keccak256, map::B256Map};
use rayon::prelude::*;
use reth_chainspec::ChainSpec;
use reth_consensus::{Consensus as _, FullConsensus, HeaderValidator as _};
use reth_errors::{BlockExecutionError, ConsensusError, ProviderError};
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives::{Block, EthPrimitives, GotExpected, RecoveredBlock, SealedHeader};
use reth_ress_protocol::ExecutionWitness;
use reth_revm::state::Bytecode;
use reth_trie::{HashedPostState, KeccakKeyHasher};
use reth_trie_sparse::{blinded::DefaultBlindedProviderFactory, SparseStateTrie};
use reth_zk_ress_protocol::ExecutionProof;
use std::time::Instant;
use tracing::*;
use zk_ress_primitives::ExecutionWitnessPrimitives;
use zk_ress_provider::ZkRessProvider;

mod root;
use root::calculate_state_root;

pub trait BlockVerifier: Unpin {
    type Proof: ExecutionProof;

    fn verify(&self, block: RecoveredBlock<Block>, proof: Self::Proof)
        -> Result<(), VerifierError>;
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
    provider: ZkRessProvider<ExecutionWitnessPrimitives>,
}

impl ExecutionWitnessVerifier {
    /// Create new execution witness block verifier.
    pub fn new(provider: ZkRessProvider<ExecutionWitnessPrimitives>) -> Self {
        Self { provider }
    }
}

impl BlockVerifier for ExecutionWitnessVerifier {
    type Proof = ExecutionWitness;

    fn verify(
        &self,
        block: RecoveredBlock<Block>,
        proof: Self::Proof,
    ) -> Result<(), VerifierError> {
        let chain_spec = self.provider.chain_spec();

        let evm_config = EthEvmConfig::new(chain_spec.clone());

        // TODO: Merge these two `ExecutionWitness` types
        let execution_witness = reth_stateless::ExecutionWitness {
            state: proof.state,
            codes: proof.codes,
            keys: proof.keys,
            headers: proof.headers,
        };
        reth_stateless::validation::stateless_validation(
            block.clone_block(),
            execution_witness,
            chain_spec,
            evm_config,
        )
        .map_err(|err| VerifierError::Other(Box::new(err)))?;

        Ok(())
    }
}
