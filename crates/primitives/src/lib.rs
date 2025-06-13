//! Ress primitive types.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use alloy_primitives::Bytes;
use alloy_rlp::{BytesMut, Decodable, Encodable as _};
use reth_ress_protocol::ExecutionWitness;
use reth_zk_ress_protocol::ExecutionProof;
use std::fmt;

pub mod witness;
pub mod witness_rpc;

pub trait ZkRessPrimitives: 'static {
    type Proof: ExecutionProof
        + TryFromNetworkProof<Self::NetworkProof>
        + TryIntoNetworkProof<Self::NetworkProof>
        + fmt::Debug;
    type NetworkProof: ExecutionProof;
}

pub trait TryFromNetworkProof<T>: Sized {
    /// The type returned in the event of a conversion error.
    type Error: fmt::Debug;

    /// Performs the conversion.
    fn try_from(value: T) -> Result<Self, Self::Error>;
}

pub trait TryIntoNetworkProof<T> {
    /// The type returned in the event of a conversion error.
    type Error: fmt::Debug;

    /// Performs the conversion.
    fn try_into(self) -> Result<T, Self::Error>;
}

#[derive(Clone, Debug)]
pub struct ExecutionWitnessPrimitives;

impl ZkRessPrimitives for ExecutionWitnessPrimitives {
    type Proof = ExecutionWitness;
    type NetworkProof = Bytes;
}

impl TryFromNetworkProof<Bytes> for ExecutionWitness {
    type Error = alloy_rlp::Error;

    fn try_from(value: Bytes) -> Result<Self, Self::Error> {
        ExecutionWitness::decode(&mut &value[..])
    }
}

impl TryIntoNetworkProof<Bytes> for ExecutionWitness {
    type Error = alloy_rlp::Error;

    fn try_into(self) -> Result<Bytes, Self::Error> {
        let mut encoded = BytesMut::new();
        self.encode(&mut encoded);
        Ok(encoded.freeze().into())
    }
}
