# The `rosetta-wine` engine

`rosetta-wine` runs x86-64 Wine under Rosetta 2 with DXMT for DirectX 10 and
11, vkd3d-proton over a Vulkan-on-Metal driver for DirectX 12, and optional
user-supplied D3DMetal. The crate `applejack-engine` holds the engine domain
model (the `Engine` trait, the bundle descriptor, installation, version
management, the launch environment allowlist) and the implementation,
`RosettaWine` in `src/rosetta_wine/`: the prefix builder, launching and
sessions, process discovery, the D3DMetal overlay, snapshots, and the CPUID
probe. Rosetta runs the x86-64 loader directly; nothing is wrapped in
`arch -x86_64`.

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
`..` links that stay inside install as they are. The walk counts every link
met in one lookup against the macOS limit of 32 (`MAXSYMLINKS`, past which
the kernel fails with `ELOOP`), and the kernel must agree: a link the walk
accepts but `canonicalize` cannot follow is refused, so an installed bundle
never holds a link that does not resolve. Validation then
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
   real directory (not a symlink) with a regular `BUNDLE.toml`.
2. Opens the archive once and hashes it with SHA-256 against the request's
   digest. On mismatch it stops before touching anything on disk and leaves
   the archive in place, reporting expected and actual digests. The same
   open handle is rewound and extracted, so the bytes verified are the bytes
   unpacked.
3. Removes a stale `engines/<id>/<version>` without a bundle (a directory,
   file, or symlink left by an interrupted removal or placed by hand; a
   symlink is unlinked, its target untouched) and a stale
   `engines/<id>/<version>.partial` from an earlier attempt, whatever it is,
   then extracts into a fresh `.partial`. An entry whose path or link target
   would leave the root is refused. Removing a large stale directory is a
   one-time cost paid here, after the archive has verified. The launcher
   does not detect a volume mounted at `<version>`; mounting one there is a
   deliberate user action, and its contents would be removed with the stale
   directory.
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
- `versions::list` returns installed versions, oldest first. Only a real
  directory (not a symlink) whose name is a version and that holds a regular
  `BUNDLE.toml` counts; anything else under `engines/<id>/` is ignored and
  never pruned. That is the one definition of "installed", shared through
  `bundle::is_bundle_dir`: `InstalledEngine::open` reports `NotInstalled`
  for a `<version>/` without a descriptor or a symlinked `<version>`,
  `install` replaces either (unlinking a symlink, not its target), and
  `remove` deletes it.
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

`WINEPREFIX` and `APPLEJACK_SESSION` are required and strict: `WINEPREFIX`
must resolve to the prefix directory itself (any other path inside it would
make Wine create a fresh prefix there) and `APPLEJACK_SESSION` must be
non-empty (an empty id would match nothing, or everything, in process
discovery). `assemble` returns `Error::MissingEngineVariable` when either
has no accepted value after the tiers are applied; the error says whether
the engine did not set the variable or set a value that was rejected, and
carries the `DropReason` in the latter case. Without `WINEPREFIX` Wine would
create or mutate `~/.wine`; without the session id process discovery finds
nothing. Only the engine can satisfy a required row, so a host value never
papers over the gap. The same table is the `ALLOWLIST` rustdoc.

| Variable | Owner | Check | Roots | Required |
| --- | --- | --- | --- | --- |
| `PATH`, `HOME`, `TMPDIR` | host | any value | | |
| `WINEPREFIX` | engine | one absolute path resolving to the prefix directory itself | prefix | yes |
| `WINEDEBUG` | game | any value | | |
| `WINEDLLOVERRIDES`, `WINEMSYNC`, `WINEESYNC` | engine | any value | | |
| `DXMT_CONFIG` | game | any value: inline `key=value;` options | | |
| `DXMT_CONFIG_FILE` | game | one absolute path to a config file | engine, overlay, prefix | |
| `DXMT_LOG_LEVEL`, `MTL_HUD_ENABLED`, `ROSETTA_ADVERTISE_AVX` | game | any value | | |
| `GST_PLUGIN_PATH` | game | colon-separated absolute paths | engine, overlay, prefix | |
| `APPLEJACK_SESSION` | engine | any non-empty value | | yes |
| `VK_ICD_FILENAMES` | game | colon-separated absolute paths | engine, overlay, prefix | |

`WINEARCH` is deliberately not a row. The prefix architecture is fixed by
the loader: a 64-bit `wine` creates a `win64` prefix, and `WINEARCH` only
forces a 32-bit one, which no engine creates. Prefix creation therefore
runs `wineboot --init` through `assemble` like any launch, with no extra
variable.

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
every root. A trailing `/` or `/.` is dropped before the check (`lstat`
follows a link spelled `later/`, which would make a dangling link look like
a missing component), and the same normalization applies to the roots
themselves, so a root spelled `<link>/` is still refused as a symlink. Only
components that do not exist at all are re-appended lexically. Any `DYLD_*`
variable is always dropped; the hardened runtime
strips them anyway, so the bundle must be self-contained. Names outside the
table are dropped.

## Prefix builder

`RosettaWine::create_prefix(installed, prefix_dir, spec)` requires an
absolute `prefix_dir` that is absent or an empty directory, refuses a backend
the bundle cannot serve (see "Backends"), and then runs the four steps of
`rosetta_wine::prefix::PREFIX_STEPS`, in this order:

| Step | What happens |
| --- | --- |
| `wineboot` | `<loader> wineboot --init` with the environment below, cwd the prefix, stdout and stderr to `<spec.log_dir>/<session>/wineboot.log`; then `<server> -w`, bounded by `SERVER_WAIT` (180 s), after which the server is killed with `-k` and the step fails |
| `dll-overrides` | the backend's DLLs from `[overrides.<backend>]` copied into `drive_c/windows/system32` (`x86_64` list) and `drive_c/windows/syswow64` (`i386` list); the files copied are recorded so the next apply removes them first |
| `registry-seeds` | a `REGEDIT4` file imported with `<loader> regedit /S seeds.reg` from the directory holding it (a relative file name needs no path translation), then `<server> -w`: `HKCU\Software\Wine\WineDbg\ShowCrashDialog = 0`, `HKCU\Software\Wine\DllOverrides\<dll> = "native,builtin"` for the backend's DLLs and the value deleted (`=-`) for every other backend's, and the HID toggle at the bundle's `[registry] hidraw_key` (the last path component is the value name; `HKLM`/`HKCU` are expanded): `DisableHidraw = 1` for `hidraw = disabled`, `0` for `enabled` |
| `library-drive` | `dosdevices/s:` symlinked to `spec.library_dir` when set |

A failure in any step removes the prefix directory and returns the error with
the log path (`Error::WineCommandFailed { command, status, log }`); the
`wineboot.log` lives under the log root, not the prefix, so it survives.
Prefix creation is a session like a launch: `APPLEJACK_SESSION=prefix-<id>`
and `WINEPREFIX` are set through `assemble`, so the allowlist's required rows
hold for it too.

The engine's own state in a prefix lives under `<prefix>/applejack/`:
`prefix.toml` (`PrefixRecord`: engine, engine version, backend, sync mode,
hidraw, the copied DLL paths), the last `seeds.reg`, and the `regedit.log` of
the last `apply_overrides`. `RosettaWine::open_prefix(dir)` reads the record
back into a `Prefix`; a record naming another engine is refused.
`apply_overrides(installed, prefix, backend, sync_mode, hidraw)` re-runs the
`dll-overrides` and `registry-seeds` steps (removing the previously copied
DLLs first, so a switch from DXMT to vkd3d-proton leaves no `winemetal.dll`
behind) and rewrites the record; it never re-runs `wineboot`. Wine runs its
own prefix update when a newer Wine opens an older prefix; taking a snapshot
before that (the plan's "snapshot before an engine-version change") is the
core's call, through the snapshot API below, since the engine does not know
the snapshot root.

### Backends

- `wined3d`: no DLL overrides.
- `dxmt`: available when `[overrides.dxmt]` lists a DLL.
- `vkd3d-proton`: available when `[overrides.vkd3d]` lists a DLL; the
  launch also sets `VK_ICD_FILENAMES` to `<bundle>/vulkan/MoltenVK_icd.json`
  when that file exists (a conventional path until `BUNDLE.toml` names the
  ICD; recorded as a follow-up).
- `d3dmetal`: available only when `[d3dmetal]` names an overlay location
  and the overlay holds both `D3DMetal.framework` and `libd3dshared.dylib`
  ("verified" means imported through the command below; assumption A5, that
  the payload loads in the project's Wine, is gate G4d and is not checked
  here). Otherwise `create_prefix` and `apply_overrides` refuse with
  `Error::BackendUnavailable` before touching anything.

## The environment the children see

Every Wine process the engine starts (wineboot, regedit, wineserver, a
launch) gets its environment from `launch::env::assemble` with the engine
tier built by `rosetta_wine::launch::engine_rows`:

| Variable | Value |
| --- | --- |
| `PATH` | `<bundle>/<dir of wine.loader>:/usr/bin:/bin:/usr/sbin:/sbin` (`HOST_PATH`). Neither `/usr/local/bin` nor `/opt/homebrew/bin` is on it: `wineboot` runs `update-mime-database` when it finds one on `PATH`, and the payload must not pick up host tools. |
| `WINEPREFIX` | the prefix directory |
| `APPLEJACK_SESSION` | `launch-<nanos>-<pid>-<n>`, `prefix-...`, or `apply-...` |
| `WINEDLLOVERRIDES` | `mscoree=;mshtml=` always (`MONO_GECKO_OVERRIDE`), then `;<dll>,<dll>=n,b` for the backend's DLLs |
| `WINEESYNC=1` or `WINEMSYNC=1` | from the prefix's sync mode; nothing for `none` |
| `VK_ICD_FILENAMES` | the MoltenVK ICD, vkd3d-proton backend only |
| `ROSETTA_ADVERTISE_AVX=1` | only when `LaunchSpec::advertise_avx` and the recorded `CpuidProbe` says the variable changes the advertised bits |

`HOME` and `TMPDIR` come from the host environment; `WINEDEBUG` and the other
game-owned rows come from the host or `LaunchSpec::env_extras`. Variables
dropped by the allowlist are written to the session log with their reason
before the loader starts.

Wine Mono and Gecko: `wineboot --init` on this payload blocks on the
"install Wine Mono" dialog unless `mscoree` is disabled (the first run on
this host sat for 2.5 hours in `control.exe appwiz.cpl install_mono`).
applejack never ships Wine Mono or Gecko, so the override is permanent and
present in every launch, not only during prefix creation; a title that needs
.NET or the embedded browser fails the same way with or without the dialog.
The unit test `engine_rows_carry_the_mono_gecko_override_and_backend_dlls`
asserts it for every backend and sync mode.

## Launch and sessions

`RosettaWine::launch(installed, prefix, spec)` refuses a prefix of another
engine and a 32-bit title on a `wow64 = "none"` bundle (it reads the PE
machine type of `spec.exe`, resolving `X:\...` through
`<prefix>/dosdevices/x:` and relative paths against `spec.cwd`; a file it
cannot read passes, and Wine reports its own error), then spawns
`<loader> <exe> <args>` with cwd `spec.cwd`, stdin closed, and stdout and
stderr appended to `<spec.log_dir>/<session-id>/wine.log`. The launcher's
log root is `~/Library/Logs/applejack`; tests pass a temporary directory.
The `Session` owns the child handle and offers:

- `wait()` / `try_wait()`: the loader's exit status (the loader `exec`s into
  the Windows process, so its pid and status are the title's).
- `cancel()`: `<server> -k` then `<server> -w` against the prefix, so it
  returns once the server and every process of the prefix are gone.
- `processes()`: same-uid processes carrying the session id (below).
- `wineserver_alive()`: whether a server listens for the prefix.

## Process discovery

Wine passes the environment to every Windows process it starts, so
`APPLEJACK_SESSION=<id>` survives its forks and reparenting. The kernel
exposes a process's exec path, argv, and environment through
`sysctl(KERN_PROCARGS2)`. The crate choice is `sysinfo` (0.38,
`default-features = false, features = ["system"]`): it is the one safe
wrapper over that `sysctl` in the local registry, the workspace forbids
`unsafe` so `libc::sysctl` cannot be called directly, and a dedicated FFI
crate would be out of scope. `rosetta_wine::process::find_session(id)`
refreshes every process's user, exe, cmd, and environ through it, keeps the
ones owned by the current uid (`rustix::process::getuid`) whose environment
holds the exact `APPLEJACK_SESSION=<id>` entry, and reports
`Process { pid, exe, session_id }`. `exe` is the last argv entry naming a
`.exe`, reduced to its file name (`steam.exe`, `steamwebhelper.exe`, the
game); Wine rewrites argv in place so argv[0] is the Windows executable, and
`KERN_PROCARGS2` reads the live argument area, so `ps` and this agree. The
matching is `match_record`, unit-tested over a captured record.
`ps -E` remains the debugging fallback.

`wineserver_alive(prefix_dir)` derives the server's socket as Wine does,
`/tmp/.wine-<uid>/server-<dev>-<inode>/socket` with the prefix directory's
device and inode in hex, and connects to it: a connect succeeds only while
the server is alive, so a stale socket left by a killed server reads as not
alive, and the server drops the unused connection.

## D3DMetal overlay import

`rosetta_wine::d3dmetal::import(installed, source, accept_apple_license)`
is a function over a path: `source` is the root of a mounted Game Porting
Toolkit volume (a fixture tree in tests; never the Gcenx cask, and never
run in CI). It checks, in order:

1. the bundle declares `[d3dmetal] overlay_dir` (else
   `Error::D3DMetalUnsupported`);
2. `<source>/README.md` is readable (else `Error::D3DMetalSource` naming
   that path);
3. `<source>/redist/lib/external/D3DMetal.framework` and
   `libd3dshared.dylib` exist (assumption A1; else `Error::D3DMetalSource`
   naming the missing entry and the README to consult);
4. `accept_apple_license` is set (else `Error::LicenseNotAccepted` carrying
   the acknowledgement text, which names `<source>/License.pdf` and the flag
   `--accept-apple-license`).

It then replaces `<overlay_root>/<overlay_dir>/` with a copy of the two
entries (symlinks inside the framework kept as links) and returns
`D3DMetalOverlay { overlay_dir, imported_from, gptk_version, accepted_at }`
for the core to store in its `d3dmetal` table; `gptk_version` is the first
numeric token after "Game Porting Toolkit" in the README, or `unknown`.
`remove(installed)` deletes the overlay. Wiring the overlay into Wine's
library search (the A5 mechanism) is the payload build's decision and is not
done here. D3DMetal is never committed, downloaded, or published.

## Snapshots and retention

`rosetta_wine::snapshot` works on directories, independent of the engine:

- `create(prefix_dir, snapshots_root, name)` copies the prefix to
  `<snapshots_root>/<name>/<YYYYMMDDTHHMMSSZ>/` (with `-N` appended when
  the second is already taken) using `/bin/cp -c -R` (an APFS clone;
  `cp` itself falls back to a byte copy across volumes), and `cp -R` when
  `-c` is refused. Symlinks are copied as links, so `dosdevices/z: -> /` is
  not followed.
- `list(snapshots_root, name)` returns them oldest first.
- `restore(prefix_dir, snapshots_root, name, timestamp)` takes a safety
  snapshot of the current prefix, copies the chosen one to
  `<prefix_dir>.restoring`, and swaps it in with two renames; an interrupted
  restore leaves the prefix as it was or fully restored. Returns the safety
  snapshot.
- `reset(prefix_dir, snapshots_root, name, keep_user_data, recreate)` takes
  a safety snapshot, removes the prefix, calls `recreate` (the core wraps
  `create_prefix`), and with `keep_user_data` copies `drive_c/users/` back
  from the safety snapshot.
- `prune(snapshots_root, name, keep)` removes all but the newest `keep`;
  `DEFAULT_KEEP` is 3.

## CPUID probe

`RosettaWine::probe_cpuid(installed)` runs `[probes] cpuid` twice, without
and with `ROSETTA_ADVERTISE_AVX=1`, with only `PATH` in the environment, and
returns `CpuidProbe { plain, advertised }` of `CpuidFeatures { avx, avx2,
osxsave, xcr0_ymm }`. `changes_bits()` is what a launch consults
(`LaunchSpec::cpuid_probe`) before setting the variable. Storing the result
in `meta` is the core's job.

## Doctor and install

`doctor(host, installed)` is read-only: Rosetta presence (error when the
core says it is absent, warning when it could not tell, informational on an
x86-64 host), macOS against `min_macos`, a warning below macOS 15 when the
bundle carries DXMT, the loader and server present, the probes present, the
backends and sync modes, and whether the D3DMetal overlay is imported.
`install` refuses with `Error::RosettaMissing` on an arm64 host whose
`rosetta_present` is `Some(false)` and otherwise delegates to
`bundle::install`.

## What the CLI unit will expose

The commands in the plan map onto this crate as follows; none exist yet.

| Command | Calls |
| --- | --- |
| `applejack engine doctor` | `RosettaWine::doctor` with the core's `Host` |
| `applejack engine install rosetta-wine` | `RosettaWine::install` with an `InstallRequest` |
| `applejack engine probe-cpuid` | `RosettaWine::probe_cpuid`, stored in `meta` |
| `applejack engine d3dmetal import <path> [--accept-apple-license]` | `d3dmetal::import`, record stored in `d3dmetal` |
| `applejack engine d3dmetal remove` | `d3dmetal::remove`, record cleared |
| `applejack prefix create <name> --backend <b>` | `RosettaWine::create_prefix` with `log_dir = ~/Library/Logs/applejack` |
| `applejack prefix snapshot <name>` / `restore` / `reset [--keep-user-data]` / `snapshot prune` | `snapshot::{create, restore, reset, prune}` under `prefixes/.snapshots/` |
| `applejack run <appid>` | `RosettaWine::launch`, then `Session::{wait, cancel, processes, wineserver_alive}` |

## Known limits

- Wine Mono and Gecko are never installed; .NET titles and embedded
  browsers that need them do not work in this engine.
- msync is not in the payload (gate G4b deferred); `sync_modes` is
  `["esync", "none"]`.
- `VK_ICD_FILENAMES` is set from a conventional path until `BUNDLE.toml`
  names the ICD.
- The D3DMetal backend is listed as available as soon as the overlay is
  imported; A5 (the payload loads in the project's Wine) is verified by gate
  G4d, not by the engine.
