//! Block verifier for `zk-ress` stateless client.

use alloy_primitives::{keccak256, map::B256Map};
use alloy_rlp::{Decodable, Encodable};
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
use reth_zk_ress_protocol::ExecutionProof;
use std::time::Instant;
use tracing::*;
use zk_ress_evm::BlockExecutor;
use zk_ress_provider::ZkRessProvider;

mod root;
pub use root::calculate_state_root;

/// Multi proof wrapper that can contain different proof types.
/// This allows the verifier to handle multiple proof types from different network protocols.
#[derive(Debug, Clone)]
pub enum MultiProof {
    /// Execution witness proof from the "zk-ress-execution-witness" protocol.
    ExecutionWitness(ExecutionStateWitness),
}

impl Default for MultiProof {
    fn default() -> Self {
        Self::ExecutionWitness(ExecutionStateWitness::default())
    }
}

impl ExecutionProof for MultiProof {
    fn is_empty(&self) -> bool {
        match self {
            Self::ExecutionWitness(proof) => proof.is_empty(),
        }
    }
}

impl Encodable for MultiProof {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        match self {
            Self::ExecutionWitness(proof) => {
                // Encode prover byte (0 for ExecutionWitness) followed by the proof
                out.put_u8(0);
                proof.encode(out);
            }
        }
    }
}

impl Decodable for MultiProof {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        // Read the prover byte to determine proof type
        if buf.is_empty() {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        
        let prover_byte = buf[0];
        *buf = &buf[1..]; // Skip the prover byte
        
        match prover_byte {
            0 => {
                // ExecutionWitness
                let proof = ExecutionStateWitness::decode(buf)?;
                Ok(Self::ExecutionWitness(proof))
            }
            1 => {
                // SP1 - TODO: Implement when SP1 proof type is available
                return Err(alloy_rlp::Error::Custom("SP1 proof decoding not yet implemented"));
            }
            2 => {
                // Risc0 - TODO: Implement when Risc0 proof type is available  
                return Err(alloy_rlp::Error::Custom("Risc0 proof decoding not yet implemented"));
            }
            _ => {
                return Err(alloy_rlp::Error::Custom("Unknown prover type"));
            }
        }
    }
}

impl MultiProof {
    /// Create a MultiProof from a protocol name and payload.
    pub fn from_protocol_name(protocol_name: &str, proof: ExecutionStateWitness) -> Result<Self, VerifierError> {
        match protocol_name {
            "zk-ress-execution-witness" => Ok(Self::ExecutionWitness(proof)),
            _ => Err(VerifierError::Other(
                format!("Unknown protocol: {}", protocol_name).into()
            )),
        }
    }
}

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

/// Multi verifier that can handle multiple proof types.
/// Routes verification to the appropriate concrete verifier based on the proof type.
#[derive(Debug)]
pub struct MultiVerifier {
    execution_verifier: Option<ExecutionWitnessVerifier>,
}

impl MultiVerifier {
    /// Create new multi verifier with all verifiers set to None.
    pub fn new() -> Self {
        Self { execution_verifier: None }
    }

    /// Create new multi verifier with the provided execution witness verifier.
    pub fn with_execution_witness(execution_verifier: ExecutionWitnessVerifier) -> Self {
        Self { execution_verifier: Some(execution_verifier) }
    }

    /// Set the execution witness verifier.
    pub fn set_execution_witness(&mut self, verifier: ExecutionWitnessVerifier) {
        self.execution_verifier = Some(verifier);
    }
}

impl BlockVerifier for MultiVerifier {
    type Proof = MultiProof;

    fn verify(
        &self,
        block: RecoveredBlock<Block>,
        parent: SealedHeader,
        proof: Self::Proof,
    ) -> Result<(), VerifierError> {
        match proof {
            MultiProof::ExecutionWitness(witness_proof) => {
                match &self.execution_verifier {
                    Some(verifier) => verifier.verify(block, parent, witness_proof),
                    None => Err(VerifierError::Other(
                        "ExecutionWitness verifier has not been initialized".into()
                    )),
                }
            }
        }
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
        let block_num_hash = block.num_hash();

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

        // ===================== Execution =====================
        let start_time = Instant::now();
        let block_executor =
            BlockExecutor::new(self.provider.clone(), block.parent_num_hash(), &trie, &bytecodes);
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
