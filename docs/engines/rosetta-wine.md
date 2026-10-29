# The `rosetta-wine` engine

`rosetta-wine` runs x86-64 Wine under Rosetta 2 with DXMT for DirectX 10 and
11, vkd3d-proton over a Vulkan-on-Metal driver for DirectX 12, and optional
user-supplied D3DMetal. The crate `applejack-engine` holds the engine domain
model: the `Engine` trait, the bundle descriptor, installation, version
management, and the launch environment allowlist. The engine implementation
itself (prefix creation, launching, process discovery, D3DMetal import) is a
separate unit; see "What comes next".

## Bundle layout

An engine bundle is a `.tar.zst` archive. Installed, it lives at
`<data_dir>/engines/rosetta-wine/<version>/`:

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
| `engines/rosetta-wine/ACTIVE` | the selected version, one line |

Every path handed to Wine is inside the data directory or the game library
directory. D3DMetal is never committed, downloaded, or published.

## `BUNDLE.toml`

Parsed with unknown fields rejected. Every path is relative to the bundle root
and may not contain `..`; absolute paths are rejected.

| Field | Type | Notes |
| --- | --- | --- |
| `id` | `"rosetta-wine"` or `"native-arm64"` | must match the engine being installed |
| `version` | string | must match the version being installed and names the directory; no `/`, no leading `.`, no `.partial` or `.user` suffix |
| `arch` | `"x86_64"` or `"arm64"` | |
| `min_macos` | string | for example `"14.0"` |
| `sync_modes` | list of `"msync"`, `"esync"`, `"none"` | ordered, non-empty, no duplicates; the first is the default |
| `notarization_submission_id` | string | empty until notarized |
| `[wine] version`, `source_url`, `source_rev` | string, `https://` URL, string | provenance of the Wine build |
| `[wine] loader`, `server` | path | must exist as files; for example `wine/bin/wine64`, `wine/bin/wineserver` |
| `[wine] preloader` | path, optional | must exist when set |
| `[wine] wow64` | `"new"`, `"32on64"`, `"none"` | 32-bit support is reported only for `"new"` and `"32on64"` |
| `[components.<name>] version`, `license` | string, SPDX id | one entry per third-party part: `dxmt`, `vkd3d_proton`, `moltenvk`, `kosmickrisp`, `gstreamer` |
| `[overrides.dxmt] dlls`, `[overrides.vkd3d] dlls`, `[overrides.d3dmetal] dlls` | list of paths | DLLs a backend copies into the prefix and overrides |
| `[d3dmetal] overlay_dir` | path | where the user-supplied payload goes, relative to the `<version>.user/` overlay root |
| `[probes] cpuid`, `dx11_sample`, `xinput_check` | path | diagnostic executables in the bundle |
| `[registry] hidraw_key` | string | the `winebus.sys` key that toggles raw HID access in the pinned Wine |
| `[entitlements."<executable>"]` | list of strings | code-signing entitlements each executable needs |

Override DLL rule: every entry under `[overrides.*] dlls` is a path relative to
the bundle root (`dxmt/d3d11.dll`) and must exist as a file. One rule serves
all backends; the DLL name for `WINEDLLOVERRIDES` is the file stem. The
`d3dmetal` table lists bundle-side DLLs only; the native D3DMetal payload lives
in the overlay and is not validated here.

Capabilities derived from the descriptor: `WineD3D` is always available;
`Dxmt` and `Vkd3dProton` when their override tables are non-empty; `D3DMetal`
when `[d3dmetal]` is present. Whether the overlay is populated is checked at
run time.

## Installation

`bundle::install` (and `install_from_archive` for a local file):

1. Refuses when `engines/<id>/<version>/` already exists.
2. Hashes the archive with SHA-256 and compares it with the pinned digest from
   `engines/manifest.toml`. On mismatch it stops before extracting anything and
   leaves the archive in place, reporting expected and actual digests.
3. Extracts into `engines/<id>/<version>.partial/`, replacing a stale partial
   directory from an earlier attempt.
4. Loads and validates `BUNDLE.toml` and checks that it names the same engine
   and version. On any failure the partial directory is removed.
5. Renames `.partial` to `<version>/`.

No other version directory is read or written. A download source is added as
another `ArchiveSource` variant without changing callers.

## Versions and prune

- `versions::list` returns installed versions, oldest first. Version order
  compares dot- and dash-separated segments, numbers by value and other text
  lexically: `9.9.9 < 10.0.9 < 10.0.10`, and `10.0.0-rc1 < 10.0.0`.
- `versions::active` and `versions::set_active` read and write
  `engines/<id>/ACTIVE`. Selecting a version that is not installed fails. A
  recorded version whose directory has disappeared counts as no selection.
- `versions::remove` deletes one version and refuses the active one or one a
  game configuration references.
- `versions::prune(keep, referenced)` keeps the active version, every
  referenced version, and the newest `keep` others, then removes the rest and
  returns what it removed. The launcher default is `keep = 1`, so active plus
  previous stay. Prune never touches `<version>.user/` overlays because they
  hold user-supplied files.

## Launch environment allowlist

Every launch builds the child environment from the host environment and the
per-game extras through one table (`launch::env::ALLOWLIST`). Extras override
the host value of the same name. Anything else is dropped and reported with a
reason so it can be logged.

| Variable | Check |
| --- | --- |
| `PATH`, `HOME`, `TMPDIR` | any value |
| `WINEPREFIX` | one absolute path inside an allowed root |
| `WINEDEBUG`, `WINEDLLOVERRIDES`, `WINEMSYNC`, `WINEESYNC` | any value |
| `DXMT_CONFIG` | one absolute path inside an allowed root |
| `DXMT_LOG_LEVEL`, `MTL_HUD_ENABLED`, `ROSETTA_ADVERTISE_AVX` | any value |
| `GST_PLUGIN_PATH` | colon-separated absolute paths, each inside an allowed root |
| `APPLEJACK_SESSION` | any value |
| `VK_ICD_FILENAMES` | colon-separated absolute paths, each inside an allowed root |

Allowed roots are the engine directory, its `<version>.user` overlay, and the
prefix directory. The check is lexical: the path must be absolute, contain no
`..`, and start with one of the roots. Any `DYLD_*` variable is always dropped;
the hardened runtime strips them anyway, so the bundle must be self-contained.
Names outside the table are dropped.

## What comes next

The `rosetta-wine` implementation of the `Engine` trait adds:

- `doctor`: Rosetta presence, macOS version against `min_macos`, bundle state.
- Prefix creation: `wineboot --init`, `WINEARCH=win64`, backend DLL overrides,
  sync mode variables, registry seeds including the `hidraw_key`, the `S:`
  library drive, snapshots before an engine version change.
- Launch: spawning the loader with `APPLEJACK_SESSION`, log capture to
  `~/Library/Logs/applejack/<session>/wine.log`, cancellation through
  `wineserver -k`.
- Process discovery: same-user processes carrying the session id, and the
  `wineserver` liveness check for a prefix.
- D3DMetal: import from a mounted Game Porting Toolkit image into the overlay
  with license acknowledgement, and removal.
- The CPUID probe that records whether `ROSETTA_ADVERTISE_AVX=1` changes the
  advertised features.
