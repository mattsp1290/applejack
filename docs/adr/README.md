# Architecture decision records

One record per key decision in the macOS Windows Steam runner plan.

- [ADR-0001](ADR-0001.md) Engine-agnostic core with a versioned engine interface.
- [ADR-0002](ADR-0002.md) Rust core exposed as a CLI and a stdio JSON-RPC server.
- [ADR-0003](ADR-0003.md) Project-built, project-signed, notarized engine bundles.
- [ADR-0004](ADR-0004.md) D3DMetal is user-supplied, never downloaded or bundled.
- [ADR-0005](ADR-0005.md) Steam depots through the Rust Steam crates with a DepotDownloader fallback.
- [ADR-0006](ADR-0006.md) Client-required titles launch through the real Windows Steam client.
- [ADR-0007](ADR-0007.md) Synchronization mode comes from the bundle.
- [ADR-0008](ADR-0008.md) Native ARM64 engine is a gated track, not a blocker.
- [ADR-0009](ADR-0009.md) Rollback is a first-class engine and prefix operation.
- [ADR-0010](ADR-0010.md) The Rosetta engine builds Wine from the WineHQ 11.0 stable release.
