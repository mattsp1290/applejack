# Working in applejack

Run `cargo xtask check` before handoff. It checks formatting, Clippy with the
workspace lints, workspace tests, rustdoc with warnings denied, the engine
manifest, and the Swift package build and tests, and it must exit 0 from the
repository root.

Keep `.agents/plans/`, `.agents/reviews/`, and `reviews/` local. They are
ignored and must not be committed.

Never commit credential values, Steam tokens, or `.acf` or `.vdf` files that
contain account identifiers. Use environment variable names in documentation,
never values.

Tests run without Steam credentials. Live tests are opt-in: they require
`APPLEJACK_LIVE_STEAM=1` or `APPLEJACK_LIVE_ENGINE=1` and skip otherwise.

Engine payloads and bundles under `fixtures/` are local artifacts and must not
be committed.

D3DMetal is never committed, downloaded, or published. It is user-supplied.

Crate dependency direction is `applejack` -> `applejack-core` ->
{`applejack-config`, `applejack-steam`, `applejack-engine`} ->
`applejack-manifest`. Only `applejack-core` depends on `applejack-config`.
