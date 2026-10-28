//! Orchestration for applejack: host readiness facts, the run pipeline that
//! resolves configuration and launches a title through an engine, probes, and
//! the JSON-RPC method table served by `applejack serve --stdio`. This is the
//! only crate that depends on `applejack-config`.

pub mod host;
