//! Block verifier for `zk-ress` stateless client.

use reth_chainspec::ChainSpec;
use reth_errors::{BlockExecutionError, ConsensusError, ProviderError};
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives::{Block, RecoveredBlock, SealedHeader};
use reth_ress_protocol::ExecutionStateWitness;
use reth_zk_ress_protocol::ExecutionProof;
use tracing::*;
use zk_ress_provider::ZkRessProvider;

mod root;
pub use root::calculate_state_root;

/// Implementation of block verification.
///
/// Given the proof, it must verify full validity of the block according to Ethereum consensus.
pub trait BlockVerifier: Unpin {
    /// The type of the execution proof used for verification.
    type Proof: ExecutionProof;

    /// Verify that the block is valid.
    fn verify(
        &self,
        block: RecoveredBlock<Block>,
        parent: SealedHeader,
        proof: Self::Proof,
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
    provider: ZkRessProvider<ExecutionStateWitness>,
    consensus: EthBeaconConsensus<ChainSpec>,
}

impl ExecutionWitnessVerifier {
    /// Create new execution witness block verifier.
    pub fn new(
        provider: ZkRessProvider<ExecutionStateWitness>,
        consensus: EthBeaconConsensus<ChainSpec>,
    ) -> Self {
        Self { provider, consensus }
    }
}

impl BlockVerifier for ExecutionWitnessVerifier {
    type Proof = ExecutionStateWitness;

    fn verify(
        &self,
        block: RecoveredBlock<Block>,
        parent: SealedHeader,
        proof: Self::Proof,
    ) -> Result<(), VerifierError> {
        let chain_spec = self.provider.chain_spec();

        let evm_config = EthEvmConfig::new(chain_spec.clone());

        // TODO: Merge these two `ExecutionWitness` types
        let execution_witness = reth_stateless::ExecutionWitness {
            state: proof.state,
            codes: proof.bytecodes,
            // Keys are not used at the moment
            keys: Vec::new(),
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
