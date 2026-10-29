# The `rosetta-wine` engine

`rosetta-wine` runs x86-64 Wine under Rosetta 2 with DXMT for DirectX 10 and
11, vkd3d-proton over a Vulkan-on-Metal driver for DirectX 12, and optional
user-supplied D3DMetal. The crate `applejack-engine` holds the engine domain
model: the `Engine` trait, the bundle descriptor, installation, version
management, and the launch environment allowlist. The engine implementation
itself (prefix creation, launching, process discovery, D3DMetal import) is a
separate unit; see "What comes next".

## Bundle layout

An engine bundle is a `.tar.zst` archive whose entries sit at the archive
root: `BUNDLE.toml` is a top-level entry, not `rosetta-wine-10.0.0/BUNDLE.toml`.
An archive nested under a top-level directory is refused with a message that
says so. Installed, it lives at `<data_dir>/engines/rosetta-wine/<version>/`:

```text
BUNDLE.toml                    descriptor (below)
THIRD_PARTY_NOTICES.md         generated from BUNDLE.toml
wine/                          x86-64 Wine: bin/, lib/, lib/wine/{x86_64-unix,x86_64-windows,i386-windows}, share/wine/
wine/lib/GStreamer.framework   signed framework
dxmt/                          DXMT DLLs and native component for the pinned Wine
vkd3d/                         vkd3d-proton x86-64 DLLs
vulkan/                        MoltenVK ICD JSON and dylib; KosmicKrisp ICD when shipped
probes/                        cpuid (x86-64 Mach-O), dx11-sample (PE), xinput-check (PE)
```

Next to it:

| Path | Meaning |
| --- | --- |
| `engines/rosetta-wine/<version>.partial/` | an extraction in progress; removed on failure, renamed to `<version>/` on success |
| `engines/rosetta-wine/<version>.user/` | per-user overlay; holds user-supplied D3DMetal and is never part of the signed bundle |
| `engines/rosetta-wine/<version>.download` | an archive being downloaded; outside `.partial/`, so a download that fails its digest check is kept for inspection |

The active version is not recorded on disk here. The core owns it (in the
config database) and passes it, together with every other version it must
keep, to `versions::prune` and `versions::remove` as `protected`.

Symlinks inside a bundle may point only inside the bundle. Extraction refuses
a symlink or hard link entry whose target is absolute or climbs above the
archive root (a lexical check, fast and first). Because an earlier symlink
used as a directory (`p -> .`) can make an entry's archive path deeper than
its real one, the extracted tree is then walked and every symlink is followed
component by component on disk; each intermediate path must stay inside the
canonical `.partial` root. That refuses `a/b -> ..` followed by
`a/b/c -> ..`, a target that climbs out and back in through
`<version>.partial` (inside while checked, a sibling once renamed), dangling
links, and loops, while `wine/bin/wine64 -> ../lib/wine64-real` and other
`..` links that stay inside install as they are. Validation then
canonicalizes every file it names, including the `[probes]` executables, and
requires it to stay inside the canonical bundle root. Setuid, setgid, and
sticky bits are stripped on extraction; the executable bit is kept. Every
path handed to Wine is inside the data directory or the game library
directory. D3DMetal is never committed, downloaded, or published.

## `BUNDLE.toml`

Parsed with unknown fields rejected. Every path is relative to the bundle root
and may not contain `..`; absolute paths are rejected.

| Field | Type | Notes |
| --- | --- | --- |
| `id` | `"rosetta-wine"` or `"native-arm64"` | must match the engine being installed |
| `version` | string | must match the version being installed and names the directory; lowercase `[0-9a-z._+-]`, leading digit, no trailing `.`, no `.partial`, `.user`, or `.download` suffix |
| `arch` | `"x86_64"` or `"arm64"` | must be the architecture `id` runs: `rosetta-wine` requires `x86_64`, `native-arm64` requires `arm64` |
| `min_macos` | string | for example `"14.0"` |
| `sync_modes` | list of `"msync"`, `"esync"`, `"none"` | ordered, non-empty, no duplicates; the first is the default |
| `notarization_submission_id` | string | empty until notarized |
| `[wine] version`, `source_url`, `source_rev` | string, `https://` URL, string | provenance of the Wine build |
| `[wine] loader`, `server` | path | relative to the **bundle root**, not to `wine/`: `wine/bin/wine64` and `wine/bin/wineserver`, never `bin/wine64`; must resolve to files inside the bundle |
| `[wine] preloader` | path, optional | must resolve to a file inside the bundle when set |
| `[wine] wow64` | `"new"`, `"32on64"`, `"none"` | 32-bit support is reported only for `"new"` and `"32on64"` |
| `[components.<name>] version`, `license` | string, SPDX id | one entry per third-party part: `dxmt`, `vkd3d_proton`, `moltenvk`, `kosmickrisp`, `gstreamer` |
| `[overrides.dxmt] x86_64`, `i386`; same for `[overrides.vkd3d]` and `[overrides.d3dmetal]` | lists of paths | DLLs a backend copies into the prefix and overrides, per Windows architecture (rules below) |
| `[d3dmetal] overlay_dir` | path | where the user-supplied payload goes, relative to the `<version>.user/` overlay root; required when `[overrides.d3dmetal]` lists any DLL |
| `[probes] cpuid`, `dx11_sample`, `xinput_check` | path | diagnostic executables in the bundle |
| `[registry] hidraw_key` | string | the `winebus.sys` key that toggles raw HID access in the pinned Wine |
| `[entitlements]` | table of executable path to list of strings | code-signing entitlements each executable needs |

The entitlements table is written as a table with quoted keys, one per
executable:

```toml
[entitlements]
"wine/bin/wine64" = ["com.apple.security.cs.allow-unsigned-executable-memory"]
```

Override DLL rules: `x86_64` lists the 64-bit DLLs (copied into `system32`)
and `i386` the 32-bit ones (copied into `syswow64`); `i386` must be empty when
`wow64 = "none"`. Every entry is a path relative to the bundle root
(`dxmt/d3d11.dll`, `dxmt/i386/d3d11.dll`), must end in `.dll`, and must
resolve to a file inside the bundle. The DLL name for `WINEDLLOVERRIDES` is
the file stem, so stems are unique within one table and architecture,
ignoring case; the same stem may appear once per architecture. One rule serves
all backends. The `d3dmetal` table lists bundle-side DLLs only; the native
D3DMetal payload lives in the overlay and is not validated here.

```toml
[overrides.dxmt]
x86_64 = ["dxmt/d3d11.dll", "dxmt/dxgi.dll", "dxmt/d3d10core.dll"]
i386 = ["dxmt/i386/d3d11.dll"]
```

Capabilities derived from the descriptor: `WineD3D` is always available;
`Dxmt` and `Vkd3dProton` when their override tables list any DLL for either
architecture; `D3DMetal` when `[d3dmetal]` is present. Whether the overlay is
populated is checked at run time. `InstalledEngine` carries `overlay_root`
(`engines/<id>/<version>.user/`) next to `root` so that check and the launch
roots derive from one value.

## Installation

Every install is an `InstallRequest`: which engine and version, where the
archive comes from, the digest it must hash to, and the manifest entry it
came from, if any. Its fields are private and it has two constructors:
`InstallRequest::from_manifest(entry, source)` for a bundle pinned by
`engines/manifest.toml`, and `InstallRequest::local(id, version, archive,
sha256)` for a local, unsigned bundle (the `APPLEJACK_ALLOW_UNSIGNED_ENGINE`
path). A request that carries a manifest entry therefore always agrees with
it. `bundle::install(layout, &request, progress)` and
`Engine::install(&self, host, layout, &request, progress)` have the same
shape; the engine adds its own checks before calling the bundle step.
`rosetta-wine` refuses with `Error::RosettaMissing` when
`host.rosetta_present == Some(false)` (naming
`softwareupdate --install-rosetta`) and proceeds when it is `None`: `doctor`
reports uncertainty about the host, and an install is not blocked by a probe
the core could not run. The `Host` handed to `install` is the one `doctor`
saw, so the engine never probes the host itself.
`progress` receives `Verifying`, `Extracting`, and `Validating` in that order
(`Downloading` once a download source exists). The steps:

1. Refuses when `engines/<id>/<version>/` already holds a bundle, that is a
   regular `BUNDLE.toml`. A directory, file, or dangling link at that path
   without one is stale (an interrupted removal) and is removed first.
2. Opens the archive once and hashes it with SHA-256 against the request's
   digest. On mismatch it stops before extracting anything and leaves the
   archive in place, reporting expected and actual digests. The same open
   handle is rewound and extracted, so the bytes verified are the bytes
   unpacked.
3. Replaces a stale `engines/<id>/<version>.partial` from an earlier attempt,
   whatever it is (directory, file, or dangling link), and extracts into it.
   An entry whose path or link target would leave the root is refused.
4. Walks the extracted tree and refuses any symlink that does not resolve
   inside it (below). Checks that `BUNDLE.toml` is a top-level entry, then
   loads and validates it and checks that it names the same engine and
   version. When the request came from a manifest entry, the entry's `id`
   and `sha256` must match the request and its `arch`, `version`, and
   `min_macos` must match the descriptor. On any failure the partial
   directory is removed.
5. Renames `.partial` to `<version>/`; if the rename fails, `.partial` is
   removed as well.

No other version directory is read or written. A download source is added as
another `ArchiveSource` variant; it downloads to
`EngineLayout::download_path(id, version)` and reports through the same
callback.

## Versions and prune

- A `Version` is lowercase ASCII `[0-9a-z._+-]` with a leading digit and no
  trailing `.`, and never ends in `.partial`, `.user`, or `.download`. Byte
  equality is therefore filesystem identity on the case-insensitive default
  volume, and `10.0.0-RC1` is rejected rather than silently aliased.
- `versions::list` returns installed versions, oldest first. Only a directory
  whose name is a version and that holds a regular `BUNDLE.toml` counts;
  anything else under `engines/<id>/` is ignored and never pruned. That is
  the one definition of "installed": `InstalledEngine::open` reports
  `NotInstalled` for a `<version>/` without a descriptor, `install` replaces
  such a directory, and `remove` deletes it.
- Version order compares dot- and dash-separated segments left to right, each
  split into digit and letter runs: numbers by value (of any width, so a
  digit run wider than a machine word still sorts numerically), letters
  lexically. So
  `9.9.9 < 10.0.9 < 10.0.10`, `10.0.0-rc1 < 10.0.0`, and
  `10.0.0-rc9 < 10.0.0-rc10`. Caveats: a shorter version with a letter tag
  sorts below the plain release (`10.0.0-rc1 < 10.0.0`, and `10.0.0a` would
  too), while a numeric tag sorts above it (`10.0.0 < 10.0.0-1`, a
  post-release); tags compare lexically (`beta < rc`); `_` and `+` are not
  separators.
- `versions::remove(layout, id, version, protected)` deletes whatever sits at
  `engines/<id>/<version>`, a bundle or a stale directory. It reports
  `NotInstalled` when nothing is there (checked first, so a protected
  version that is not installed is reported as such) and refuses one in
  `protected`.
- `versions::prune(layout, id, keep, protected)` keeps every protected
  version and the newest `keep` others, removes the rest, and returns what it
  removed. `protected` is every version the caller must keep: the active one,
  those referenced by a game configuration, and those used by running
  sessions. The comparison is exact. The launcher default is `keep = 1`, so
  active plus previous stay. Prune never touches `<version>.user/` overlays
  because they hold user-supplied files.

## Launch environment allowlist

Every launch builds the child environment from three tiers through one table
(`launch::env::ALLOWLIST`): the host environment (`base`), the per-game
extras, and the values the engine computes for the launch (`engine_set`),
applied in that order with a later tier replacing an earlier value of the
same name. Each row has an owner that decides which tiers may set it:

- `host`: describes the host. The host environment or the engine may set it;
  a game preset may not.
- `game`: a tunable. Host, game, or engine may set it.
- `engine`: a launcher invariant. Only the engine may set it.

A value from a tier the owner does not accept is dropped as `NotOwner`, so a
game preset cannot replace `APPLEJACK_SESSION` (which process discovery keys
on), `WINEPREFIX`, or the sync variables. A value that fails its path check is
dropped without unsetting the earlier tier's value: a rejected extra falls
back to the base value. Everything dropped is reported with a reason and the
tier it came from so it can be logged.

`WINEPREFIX` and `APPLEJACK_SESSION` are required: `assemble` returns
`Error::MissingEngineVariable` when either has no accepted value after the
tiers are applied, whether the engine forgot it or set a value that failed
its check. Without `WINEPREFIX` Wine would create or mutate `~/.wine`;
without the session id process discovery finds nothing. Only the engine can
satisfy a required row, so a host value never papers over the gap.

| Variable | Owner | Check | Roots |
| --- | --- | --- | --- |
| `PATH`, `HOME`, `TMPDIR` | host | any value | |
| `WINEPREFIX` | engine | one absolute path | prefix only |
| `WINEDEBUG` | game | any value | |
| `WINEDLLOVERRIDES`, `WINEMSYNC`, `WINEESYNC` | engine | any value | |
| `DXMT_CONFIG` | game | any value: inline `key=value;` options | |
| `DXMT_CONFIG_FILE` | game | one absolute path to a config file | engine, overlay, prefix |
| `DXMT_LOG_LEVEL`, `MTL_HUD_ENABLED`, `ROSETTA_ADVERTISE_AVX` | game | any value | |
| `GST_PLUGIN_PATH` | game | colon-separated absolute paths | engine, overlay, prefix |
| `APPLEJACK_SESSION` | engine | any value | |
| `VK_ICD_FILENAMES` | game | colon-separated absolute paths | engine, overlay, prefix |

DXMT split, as checked against the pinned DXMT 0.80 payload (the pin ships
binaries without a README; `d3d11.dll` and `dxgi.dll` both embed
`DXMT_CONFIG` and `DXMT_CONFIG_FILE` as distinct variable names, alongside
`DXMT_LOG_LEVEL`, `DXMT_LOG_PATH`, `DXMT_SHADER_CACHE`,
`DXMT_SHADER_CACHE_PATH`, and others): `DXMT_CONFIG` carries inline options
in the DXVK style and `DXMT_CONFIG_FILE` the path to a config file. The
earlier table had `DXMT_CONFIG` as a path, which dropped every valid option
string.

Allowed roots are built with `AllowedRoots::new(engine_dir, overlay_dir,
prefix_dir)`, which rejects an empty, relative, `/`, or `..`-containing root
and a root that is itself a symlink (it would widen the root to wherever it
points), then canonicalizes the longest existing ancestor of each (so `/var`
and `/private/var` agree). A path value must be absolute and contain no
`..`; it is then resolved the same way and must start with one of the roots
its row accepts. A Wine prefix always contains `dosdevices/z: -> /`, so
`<prefix>/dosdevices/z:/etc/hosts` resolves to `/etc/hosts` and is dropped.
A component that exists but cannot be resolved, a dangling or looping
symlink such as `dosdevices/<x>:` to an unmounted volume, makes the value
unresolvable and it is dropped too: its target could appear later, outside
every root. Only components that do not exist at all are re-appended
lexically. Any `DYLD_*` variable is always dropped; the hardened runtime
strips them anyway, so the bundle must be self-contained. Names outside the
table are dropped.

## What comes next

The `rosetta-wine` implementation of the `Engine` trait adds:

- `doctor`: Rosetta presence, macOS version against `min_macos`, bundle state.
- Prefix creation: `wineboot --init`, `WINEARCH=win64`, backend DLL overrides
  copied from the `InstalledEngine` per architecture, sync mode variables,
  registry seeds including the `hidraw_key`, the `S:` library drive,
  snapshots before an engine version change. The `Prefix` records the engine
  id and version that made it and the `backend`, `sync_mode`, and `hidraw`
  last applied; `apply_overrides` returns the updated `Prefix`.
- Launch: `Engine::launch(installed, prefix, spec)` spawns `installed`'s
  loader with `WINEPREFIX` and `APPLEJACK_SESSION` in `engine_set` and
  derives `WINEMSYNC`/`WINEESYNC`, `WINEDLLOVERRIDES`, and
  `VK_ICD_FILENAMES` from `prefix.sync_mode` and `prefix.backend` (Wine
  does not persist them, so the prefix is the one record). Log capture to
  `~/Library/Logs/applejack/<session>/wine.log`, cancellation through
  `wineserver -k`.
- Process discovery: same-user processes carrying the session id, and the
  `wineserver` liveness check for a prefix.
- D3DMetal: import from a mounted Game Porting Toolkit image into
  `installed.overlay_root` with license acknowledgement, and removal.
- The CPUID probe that records whether `ROSETTA_ADVERTISE_AVX=1` changes the
  advertised features.
