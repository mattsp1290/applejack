//! Orchestration for applejack: host readiness facts, the run pipeline that
//! resolves configuration and launches a title through an engine, probes, and
//! the JSON-RPC method table served by `applejack serve --stdio`. This is the
//! only crate that depends on `applejack-config`.

pub mod config;
pub mod engines;
pub mod error;
pub mod game_config;
pub mod host;
pub mod prefixes;
pub mod rpc;
pub mod state;

pub use error::CoreError;
pub use state::Core;
