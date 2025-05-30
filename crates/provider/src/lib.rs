//! Ress provider implementation.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

/// Main provider for retrieving data.
mod provider;
pub use provider::ZkRessProvider;

/// Chain state.
mod chain_state;
pub use chain_state::ChainState;
