# Steam client runtime

The Windows Steam client runs inside one shared Wine prefix, `steam-client`,
and launches every title whose `drm` is `steam` or `unknown` (plan 05,
decision 6). applejack installs the title's depot itself (`applejack
install`) and the client adopts it from the `.acf`; the client never
downloads a game of its own, and the day it does is assumption A2 failing
(below). `docs/steam-layer.md` covers the depot side; this file is the
client.

Items marked **unverified** have not been seen on a live client yet. Every
other statement here was exercised offline against the stub bundle, or live
as the report for unit U15 records.

Live status (units U15 to U15d): on the one host tried, `install` fetched
and verified the bootstrapper, the client installed and updated itself to
build 1792618285, and with the [web helper shim](#the-web-helper-shim) in
front of `steamwebhelper.exe` the "Sign in to Steam" window is on screen
with its content drawn within about 30 s of the client's start (17 to 22 s
measured), and comes back the same way after `stop` and a second `install`.
Readiness is read from the helper's own log (below). The fresh-install
path, where the client's first self-update lays its helper down and the
client is restarted with the shim, has run against the stub only and is
marked **unverified** below. Nothing from [Launch](#launch) onward has run
against a real client: the `RunningAppID` marker, the `-applaunch`
adoption, the `libraryfolders.vdf` edits and the client restart they can
need, the `Z:` fallback path, and the A2 rules have only been exercised
against the stub bundle, and are marked **unverified** below.

## State

`applejack runtime steam status` and `runtime.steam.status` read the state
from disk every time; nothing caches it.

| State | Read from |
|---|---|
| `absent` | no `prefixes/steam-client/drive_c/Program Files (x86)/Steam/steam.exe` |
| `installing` | `applejack/client-install.json` in the prefix while another process holds the prefix lock (probed without taking it, so a status read neither locks nor settles anything) |
| `broken` | the prefix record does not open, `Steam/` exists without `steam.exe`, the install marker is there and nobody holds the lock (an install died), or a `bin/cef/cef.win*/steamwebhelper.exe` is the [web helper shim](#the-web-helper-shim) (by its marker) with no `steamwebhelper_real.exe` beside it, or with one that is itself a shim |
| `updating` | `Steam/package/steam_client_win64.manifest` (the 2024 bootstrapper installs the 64-bit client; an older one writes `steam_client_win32.manifest`) is missing (the first self-update has not finished), or `Steam/package/tmp/` exists while the client runs |
| `needs_login` | `Steam/config/loginusers.vdf` is missing or names no account with `RememberPassword "1"` (and `AllowAutoLogin` not `"0"`) |
| `ready` | it names one |

`loginusers.vdf` holds the account name and the SteamID. They are read for
the state and nothing else: not a row, not a log, not an error message.

`meta` holds three keys: `steam_client_buildid`, the `version` in
`steam_client_win64.manifest` (or `win32`) after each successful start; `steam_client_session`,
the `APPLEJACK_SESSION` the running client was started with, which every
process the client starts carries, games included; and
`steam_client_redownload`, the A2 finding when it has happened.

`status` also reports `running` (a `wineserver` for the prefix, or a
`steam.exe` under the recorded session), the build id, and the snapshot count.

## Commands

| Command | RPC | Does |
|---|---|---|
| `runtime steam install` | `runtime.steam.install` (job) | Clears the marker of an install that died, builds the prefix if needed, downloads `SteamSetup.exe`, verifies it, runs it silently, lists the shared library in `libraryfolders.vdf`, puts the web helper shim in place, starts the client (`steam.exe -noverifyfiles`) for its self-update, and leaves it at the login window; a self-update that laid the client's helper down in place of (or over) the shim gets the client restarted once with the helper stashed and the shim in front (**unverified** live: stub only; the user's first install is the first live run). A shim with no helper beside it is removed first and that one start runs without `-noverifyfiles`, so the client's file check lays the helper down (**unverified** live). With the client installed and up, only the wait for the window: it is the way to open the window for a sign-in; a helper that is not the shim under a running client (one started outside applejack) gets a `shim:` warning naming `stop`, then `install` |
| `runtime steam status [--json]` | `runtime.steam.status` | The table above |
| `runtime steam update` | `runtime.steam.update` (job) | Stops the client, snapshots the prefix, starts the client so it updates itself, records the build |
| `runtime steam repair` | `runtime.steam.repair` (job) | Refuses with `ClientRuntimeBroken` unless the runtime is `broken`. A dead install's marker is cleared first; with the client intact after that, nothing is restored. Otherwise stops the client and restores the newest usable snapshot (its prefix record opens and it holds `steam.exe`), reporting each newer snapshot it skips and why; with no usable snapshot it refuses and names `--reinstall`. The safety copy the restore takes of the state repaired away is discarded when that state had no `steam.exe`, and kept, named in the report, when it had one. Succeeds only when the runtime no longer reads `broken`. `applejack prefix restore steam-client <timestamp>` is the deliberate rollback that keeps a safety copy |
| `runtime steam repair --reinstall` | `reinstall: true` | Stops the client, removes `Steam/package/`, and runs the bootstrapper and first run again; the bootstrapper lays no `bin/cef/`, so a shim without its helper is removed at that start and the self-update lays the helper down, which the restart stashes |
| `runtime steam stop` | `runtime.steam.stop` (job) | `<server> -k` on the prefix |

Every operation takes the `steam-client` prefix lock. While a title runs
through the client the lock is held by that session, so `stop`, `update`,
and `repair` answer `PrefixBusy` until it ends. `install` is the one
command in this unit that reaches the network, for the bootstrapper and the
client's own self-update.

### Bootstrapper

`install` fetches `https://cdn.fastly.steamstatic.com/client/installer/SteamSetup.exe`
to `prefixes/steam-client/applejack/SteamSetup.exe`. The URL is unversioned,
so no hash is pinned (decision D4). The file is checked with the payload's
`probes/osslsigncode` (`osslsigncode verify -CAfile /etc/ssl/cert.pem`):
the Authenticode chain must verify against the system trust store, and the
leaf signer's `O=` must be `Valve Corp.` exactly, over RDNs split on
unescaped commas (`O=Valve Corporation`, `O=Valve Corp. Holdings`, and a
`CN` carrying `\, O=Valve Corp.` all fail). Anything else is
`BootstrapperUnverified`, which names the signer found (or "no signature
found"); the file stays where it is for inspection and nothing runs. A
payload without the tool, or without the [web helper
shim](#the-web-helper-shim), refuses with `ClientRuntimeBroken` before the
download.

The bootstrapper runs as `SteamSetup.exe /S` in the prefix, bounded by ten
minutes, and installs under `C:\Program Files (x86)\Steam`: `steam.exe`,
`bin/SteamService.exe` and `public/` only. The rest of the client, the CEF
tree under `bin/cef/` with `steamwebhelper.exe` included, arrives with the
client's own self-update at its first start.

### First run and readiness

Before the client first starts, `config/libraryfolders.vdf` is written with
the client's own folder (`C:\Program Files (x86)\Steam`) and the shared
library (`S:\`), through a sibling file and a rename, so a client that has
never run already knows the library (**unverified** live: the client may
rewrite the file on its first run, in which case the first `run` adds the
folder again and restarts the client, below).

Every client start, `install`, `update`, and a `run` that finds no client
up, first puts the [web helper shim](#the-web-helper-shim) in front of
`steamwebhelper.exe` (on a fresh install there is no `bin/cef/` yet, and
the `shim:` line says the self-update brings it), then prints that macOS
may ask whether "applejack" may use the microphone within about 15 s of the
start (the client's audio init under `winecoreaudio`, labelled with the
launcher's name; Don't Allow is fine, and macOS stores the answer). Left
unanswered, the prompt holds the client's main loop: 15 s later the client
logs `CSteamEngine::BMainLoop appears to have stalled`, to the session's
`wine.log` (the client's stderr, where unit U15d saw it), to
`logs/console_log.txt` (where U15b saw it) or to `logs/bootstrap_log.txt`,
or writes an `assert_steam.exe_*.dmp` under `dumps/`, and about 3 minutes
in it dies with `fatal stalled cross-thread pipe`. A client that exits
during the wait with that evidence written since the start is reported as
`ClientRuntimeBroken` naming the prompt (for a dump alone, as the usual
cause); one that exits without it, as `broken` pointing at
`logs/bootstrap_log.txt`.

`steam.exe -noverifyfiles` is started under a session key of its own,
recorded in `meta.steam_client_session`. `-noverifyfiles` skips the client's
check of its own files at startup (`bootstrap_log.txt` says `Verification
skipped`), which would otherwise write its helper back over the shim; the
version check against Valve's manifest, and so the self-update, still runs.
The client downloads the rest of itself from Valve. The one start without
`-noverifyfiles` is the one after the shim check found a `cef.win*`
directory with no helper to run (an orphan shim it removed, or a helper the
client's tree lost): the client's file check (`Verifying all executable
checksums`) lays the helper down, and the restart below stashes it
(**unverified** live: stub only).

Readiness is read as: `package/steam_client_win64.manifest` (or `win32`)
exists, nothing is under `package/tmp/` for five looks in a row, and the
login window exists. The window signal is the helper's own log,
`logs/webhelper.txt`: a line written since the wait began that carries
`DesktopLoginWindow_uid0` and `Created window` (or `WasHidden 0`, the line
that follows when the window is shown). The client appends to that file
across runs, so only lines past the length it had at the start count (a
file shorter than that, rotated or truncated during the wait, is read from
its start). With no such line, a `config/loginusers.vdf` write counts, and
so does a `steamwebhelper.exe` process under the key (or in the prefix):
after 90 s without the line when this call started the client, at once
when it did not (that client's line predates the wait), and never once a
stall has been reported, since the process is then the one that is stuck.
The phase line says which signal it was (`client: window: the login window
exists (logs/webhelper.txt)`). On a fresh install the self-update lays the
client's helper down under no shim (the first window is the real helper's,
black, and closes by itself); after a client update it writes its helper
over the shim. Either way, when the window is up and the helper is not the
shim, the client is stopped (`stop:`), the helper stashed (`shim:`), and
the client started once more, so the window that is left on screen is one
that draws (**unverified** live: exercised against the stub only; the
user's first install is the first live run). Ten minutes without the window
stops the client with `ClientRuntimeBroken { state: updating }`. The client
is left running for the user to sign in; the CLI returns.

The login itself is the user's, inside the client window. applejack never
passes credentials to the client.

### The web helper shim

The client draws its windows through `steamwebhelper.exe` (CEF). CEF's GPU
process renders the login page into a window the browser process owns, and
DXMT 0.80 refuses a swap chain on a window another process owns (`err:
CreateSwapChain: cross-process swapchain not supported yet`, DXMT issue
141, nothing merged upstream); Wine's Mac driver never gets the software
path's output into a foreign window either, so `-cef-disable-gpu` gives a
black window (unit U15b). Steam maps no `-cef-*` flag to CEF's
`--in-process-gpu`, so the flag goes in through a shim in front of the
helper.

The payload ships `probes/steamwebhelper-shim.exe` (x86-64 PE, built by
`crates/applejack-engine/probes/steamwebhelper-shim/build.sh` with the
pinned llvm-mingw). Before every client start, for every
`bin/cef/cef.win*/` directory under the client's `Steam/` that holds a
`steamwebhelper.exe`:

- if the helper is neither the payload's shim by SHA-256 nor any build's
  shim by its marker (every build of the shim carries the bytes
  `applejack steamwebhelper shim`), it is the client's own: it is copied to
  `steamwebhelper_real.exe` (over any older stash) and the shim written
  over `steamwebhelper.exe` (through a sibling file and a rename). The
  first time this is the client's own helper (`Applied`); after a client
  self-update it is the new helper the update wrote over the shim
  (`Reapplied`), so the stashed helper is never older than the client;
- if the helper is the payload's shim and the stash is beside it, nothing
  is written (`InPlace`);
- if the helper carries the marker but is not the payload's shim by hash,
  it is an older build of the shim: it is replaced by this payload's and
  never stashed (`Upgraded`), so a payload upgrade cannot put a shim in
  front of itself;
- a stash that is itself a shim (by hash or marker) is removed before any
  of the above: the shim would run it in a loop (unit U15d saw 3077
  helper processes from exactly that);
- if the helper is a shim (by hash or marker) with no stash beside it, it
  is an orphan with nothing to run: `status` reads the runtime as `broken`
  (`steamwebhelper_real.exe is missing beside it`), `repair` restores a
  snapshot that holds the helper and its stash when one exists (one that
  holds the orphan is skipped), and `install` is the recovery: the orphan
  is removed (`shim: ...: removed an orphan shim; the client's self-update
  lays its helper down`), that one start runs without `-noverifyfiles` so
  the client's file check lays the helper down, and the restart after the
  window stashes it and puts the shim back (**unverified** live: stub
  only). `repair --reinstall` ends the same way: the bootstrapper lays no
  `bin/cef/`, the orphan is removed at the start, and the self-update that
  the removed `package/` triggers lays the helper down;
- a stash with no `steamwebhelper.exe` in front of it gets the shim
  written back (`Rewritten`), with no file check;
- a payload without the shim refuses with `ClientRuntimeBroken` before the
  bootstrapper download and before any client start, since the window
  would not draw.

Each directory is reported under `shim:`. The check is by hash and marker,
never by size or name. The shim resolves `steamwebhelper_real.exe` beside itself,
runs it with the original command line tail unchanged plus
`--in-process-gpu`, inherits handles, waits, and exits with the child's
code; a CEF child (` --type=renderer`, ` --type=gpu-process`, ... as a
switch of its own, after a blank) that comes through it gets no extra flag. The client's own process list then shows the
shim as `steamwebhelper.exe` and the helper and its children as
`steamwebhelper_real.exe`; `app cancel` leaves both alone.

On the one host tried (Apple M4 Max, macOS 26, engine rosetta-wine 11.0-2,
DXMT), `--in-process-gpu` draws the window at the first try: fonts, the
Steam logo and the QR code render, `logs/webhelper_gpu.txt` gets no `GPU
process started` line, and `cef_log.txt` has no `EGL_BAD_ALLOC`. The other
flag set the shim can be built with (`SHIM_FLAGS="--disable-gpu
--single-process --disable-features=IsolateOrigins,site-per-process,SpareRendererForSitePerProcess"`)
was not needed and is not shipped. A debug run of the client with
`WINEDEBUG` or `DXMT_LOG_LEVEL` goes through `applejack prefix exec
steam-client --env WINEDEBUG=+seh --env DXMT_LOG_LEVEL=debug -- 'C:\Program
Files (x86)\Steam\steam.exe' -noverifyfiles` (the launch's working
directory is the prefix root, so the exe is named by its Windows path); the
pairs are the launch's game tier and anything outside the allowlist is
dropped and logged.

Not tried: the `GPUAccelWebViewsV3` registry value, which may make the shim
unnecessary; DXMT cross-process swap chain patches; whether a client
started outside applejack (the host's own habit of double-clicking
`steam.exe`) keeps the shim, since without `-noverifyfiles` the client
writes its helper back and the next applejack start stashes it again.

### Snapshots and repair

`update` snapshots the prefix before the client touches itself (decision
9); `repair` refuses unless the runtime is `broken` (plan 05 line 43 has
it restore unconditionally), since a restore of a usable client would roll
back the sign-in in `config/loginusers.vdf` and `userdata/`. `applejack
prefix restore steam-client <timestamp>` is the deliberate rollback and
keeps a safety copy. `repair` first clears the marker of an install that
died; when the client is intact after that, it restores nothing. Otherwise
it restores the newest *usable* snapshot: one whose `applejack/prefix.toml`
opens and which holds `steam.exe`. A newer snapshot that is not one (the
kept copy of a bad record, say) is reported as skipped with its reason and
never chosen, so `repair` cannot put a broken state back; with no usable
snapshot it refuses and names `repair --reinstall`. The safety copy the
restore takes of the state repaired away is discarded when that state had
no `steam.exe` (its `userdata/` goes with it), and kept as a regular
snapshot when it had one, since a prefix record that does not open may sit
over a sign-in: the report names it and `applejack prefix restore
steam-client <timestamp>` brings it back by hand. `repair` succeeds only
when the runtime no longer reads `broken`; a restore that leaves it
`broken` fails with `ClientRuntimeBroken`. A run that finds the client
updating refuses with `ClientRuntimeBroken { state: updating }`
rather than snapshotting under it (plan 05 asked for the snapshot; a
snapshot needs the server gone, which a running client is not), and that
message says to wait, not to `repair`, since `repair` would roll the update
back. `steam.cfg` with `BootStrapperInhibitAll=enable` is
the documented way to stop the client updating itself; it is **unverified**
here and nothing relies on it.

## Launch

`applejack run <appid>` for a `drm = steam` or `unknown` title:

1. The runtime must be `ready`. `absent` and `needs_login` answer
   `ClientRuntimeUnavailable`, naming `applejack runtime steam install` and
   the sign-in; `installing`, `updating`, and `broken` answer
   `ClientRuntimeBroken`.
2. The title's own prefix lock (`<appid>`) is taken so no install can start
   under it, then the `steam-client` lock, held for the whole session.
3. `appmanifest_<appid>.acf` is copied to `<library>/applejack/<appid>.acf.backup`
   and the install directory's size is measured.
4. `Steam/config/libraryfolders.vdf` gains the title's library as the client
   sees it: `S:\` when the prefix's `dosdevices/s:` is that library, else
   `Z:` plus the Unix path (**unverified**: no live client has been handed
   a `Z:` library). The entry lists the appid with its `SizeOnDisk`. The
   file is written through a sibling and a rename and round-trips through
   the VDF code; a second launch changes nothing. When the write adds a
   folder the client has not seen and a client is up, the client is
   restarted first (`wineserver -k`, then this run's `steam.exe` becomes
   the client), because the client reads the file at startup only
   (**unverified** live: both that it will not re-read the file and that
   the restart makes it adopt the folder; the stub exercises the restart).
   Listing another app under a folder the client knows restarts nothing.
5. `RunningAppID` is read once. If it already names this title, which a
   client stopped mid-title leaves behind, it is reset to `0` through the
   loader (`reg add ... /d 0 /f`) and the reset is logged, so the marker
   has to be seen to appear.
6. `steam.exe -applaunch <appid> -silent -noverifyfiles [launch_args_override]`
   is started in the prefix with this run's session key. `-silent` keeps
   the client's window down, `-noverifyfiles` skips the client's check of
   its own files (which would undo the web helper shim), not the title's.
   When no client is up, the shim check runs first, this `steam.exe`
   becomes the client, `meta.steam_client_session` is set to the run's
   key, and the client build id is recorded in `meta` after the watch, as
   after an `install`; when one is up it hands off and exits. That the
   client adopts the install and launches the title from `-applaunch` is
   **unverified**. No flag came out of the CodeWeavers notes in
   `docs/research/`: they say only that a fresh bottle fixes
   `steamwebhelper` crashes. `-no-cef-sandbox` and `-noreactlogin`
   circulate as fixes and are **unverified**.
7. `game_config.steam_overlay` and `steam_input` are not applied, and
   nothing reads them yet. The client keeps both per game in
   `userdata/<id>/config/localconfig.vdf`; the key names are
   **unverified** and writing the wrong ones would do nothing quietly, so
   the defaults from plan 05 (overlay off and Steam Input off under
   D3DMetal) are a follow-up.

### Session marker

The game-running marker is `HKCU\Software\Valve\Steam\RunningAppID`, read
every tick with `<loader> reg query` in the prefix (never from `user.reg`,
which `wineserver` writes lazily). Equal to the appid, the title runs. Any
other value, or none, defers to the fallback: a process named after the
title's launch exe under the run's key or the client's (a value naming
another title says nothing about this one; the user may have started that
one from the client's window). That the live client keeps this value, sets
it at launch, and clears it at exit is **unverified** (plan 05 asks for it
to be checked at the first live test); the stub bundle's `reg` answers
from a file.

Session start is when the marker first shows; the row goes `running`. The
client has `client_start_wait` (ten minutes; a first `-applaunch` verifies
the title's files first) to report it, else the watch ends with
`ClientLaunchTimedOut`, the client is stopped with everything in its
prefix, the title included, and the row is `failed`. Session end is when the marker clears
for two looks: the row is `exited`, `ended_at` set, `exit_code` null (the
title's status is the client's, not visible), note "launched through the
Steam client". The client stays up, and the session does not drain: the
lock is released as soon as the row closes. The CLI exits 0 for such a
session.

`app cancel` ends the title by name under both keys (`SIGTERM`, then
`SIGKILL`) and leaves the client up; with no exe known it ends every
process under the keys that is not the client's own or Wine's.
`kill_prefix` stops the whole prefix, client included. A dead owner's row is
closed as `lost` when the marker is clear, even though the client's own
processes still carry the key.

### A2: the client re-downloading, and the patch it is not

Assumption A2 (plan 00) is that the client adopts the install with a
verification pass only. Every tick the watch reads the `.acf` and, every
five seconds, the install directory. The rules are **unverified** against
a live client; the stub bundle rewrites the `.acf` the way each case is
expected to look.

A routine patch is not a refutation: an install that is behind its branch
(installed by applejack, patched by the developer since) is adopted, then
updated by the client, which is what the client does to any install that is
behind. That shows as the build moving forward: `buildid` higher than the
backup's, or `TargetBuildID` higher than `buildid` (the client writes it
beside `buildid` while the update is pending). The client is stopped and
the `.acf` restored all the same, since the depot stays applejack's to
update, but the run fails with `TitleUpdatePending`, which names
`applejack runtime steam stop` and `applejack install <appid> --update`, and
nothing is written to `meta`. Files the client had already patched are
whatever `install --update` finds on disk and verifies.

Any of these refutes A2:

- `buildid` lower than the backup's;
- `StateFlags` has an update bit (`UPDATE_REQUIRED`, `UPDATE_RUNNING`,
  `UPDATE_STARTED`, `PREALLOCATING`, `DOWNLOADING`, `STAGING`, `COMMITTING`,
  `ADDING_FILES`; `VALIDATING` and `APP_RUNNING` are fine) with the build
  unchanged and no newer `TargetBuildID`: the client is fetching the depot
  it was given;
- the install directory grew by 64 MiB or more.

Then the client is stopped, the `.acf` is restored from the backup, the
finding is written to `meta.steam_client_redownload`, and the run fails with
`ClientRedownloadRefused`, which asks the user plan 00's question: whether
criterion 5 may be rewritten so the in-prefix client keeps its own copy.

## Failure catalogue

| Failure | Code | What to do |
|---|---|---|
| Bootstrapper download failed | `DownloadFailed` | Check the network; nothing was written outside `applejack/SteamSetup.exe.partial` |
| Bootstrapper signature invalid or not Valve's | `BootstrapperUnverified` | Inspect the kept file with `osslsigncode verify`; the message names the signer |
| Payload without `probes/osslsigncode` | `ClientRuntimeBroken` | Install a payload that ships the tool |
| `SteamSetup.exe` failed or ran over ten minutes | `ClientRuntimeBroken` | The session log under `logs/` has the output; `repair --reinstall` tries again |
| First run never reached the window | `ClientRuntimeBroken { updating }` | The client was stopped; `install` again (it starts the client to the window). `repair` refuses an `updating` runtime |
| The client died about 3 minutes in with `BMainLoop appears to have stalled` in the session's `wine.log`, `logs/console_log.txt` or `logs/bootstrap_log.txt`, or with a fresh `assert_steam.exe_*.dmp` under `dumps/` | `ClientRuntimeBroken { broken }` | The macOS microphone prompt for "applejack" was left unanswered (with a dump alone, the usual cause); answer it (Don't Allow is fine; the answer is stored) and `install` again. `status` does not show this one: the client is simply gone |
| `steamwebhelper.exe` is the shim and `steamwebhelper_real.exe` is missing beside it (or is a shim too) | `ClientRuntimeBroken { broken }`, and `status` reads `broken` | `install` removes the orphan shim, starts the client once with its file check on so it lays the helper down, and restarts it with the helper stashed and the shim in front; `repair` restores a snapshot holding helper and stash when one exists; `repair --reinstall` also recovers, through the full self-update |
| Payload without `probes/steamwebhelper-shim.exe` | `ClientRuntimeBroken` | Install a payload that ships the shim; without it the login window does not draw. Refused before the bootstrapper download; `status` does not show it |
| The window is on screen but black and stays after `install` returns | none | The helper ran without the shim: check `shim:` lines in the `install` output and `logs/webhelper.txt` for `steamwebhelper_real.exe` in the `webhelper launched` line, and `cef_log.txt` for `EGL_BAD_ALLOC`. A black window that closes by itself during a fresh install is the real helper's, before the restart with the shim |
| An install died and left its marker | `ClientRuntimeBroken { broken }` | `install` again: it holds the lock, so the marker is a dead install's and is cleared first. `repair` clears the marker and, with the client intact, stops there |
| `repair` found no usable snapshot | `ClientRuntimeBroken { broken }` | None listed, or none whose prefix record opens and holds `steam.exe` (each is reported as skipped); `repair --reinstall` reruns the bootstrapper |
| Run with no client | `ClientRuntimeUnavailable { absent }` | `applejack runtime steam install` |
| Run with no account | `ClientRuntimeUnavailable { needs_login }` | `applejack runtime steam install`, sign in inside the window |
| Run while updating or broken | `ClientRuntimeBroken` | `status`; wait for an update to finish, `repair` a broken client. The message's advice follows the state |
| `repair` of a runtime that is not broken | `ClientRuntimeBroken { ready }` (or the state found) | Nothing to repair; `applejack prefix restore steam-client <timestamp>` rolls back deliberately and keeps a safety copy. An `absent` runtime is pointed at `install`; an `updating` one at waiting |
| Client reported no running title | `ClientLaunchTimedOut` | The client was stopped, with any title in it; the session log names the run. A title the client refuses to launch (ownership, region) shows here |
| The client began downloading the title | `ClientRedownloadRefused` | A2 refuted; see above |
| The client began a routine patch of the title | `TitleUpdatePending` | Not a refuted A2. `runtime steam stop`, `install <appid> --update`, run again |
| Another title runs through the client | `PrefixBusy` on `steam-client` | Wait for it, or `app cancel` it |

## Recovery

- A client that will not start: `applejack runtime steam repair` clears a
  dead install's marker and, unless the client is intact after that,
  restores the newest usable snapshot of a `broken` client; it refuses any
  other state and fails rather than leave the runtime `broken`;
  `--reinstall` keeps the prefix and user data, drops
  `Steam/package/`, and reruns the bootstrapper. The CodeWeavers note that
  every client update "breaks the bottle" and a fresh one fixes it is why
  `update` snapshots first.
- A prefix beyond repair: `applejack prefix remove steam-client`, then
  `install`. The sign-in is needed again.
- A title the client re-downloaded: the `.acf` backup under
  `<library>/applejack/` is the pre-launch state; `applejack install <appid>
  --verify` says what changed on disk.

## Not done here

- The headless Steam shim spike (`docs/spikes/headless-steam-shim.md`).
- The `BootStrapperInhibitAll` spike.
- `game_config.isolated_prefix`: a per-title client prefix with its own
  login and library.
- The per-game overlay and Steam Input toggles in `localconfig.vdf`; the
  `game_config` fields exist and nothing reads them.
