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

Live status (unit U15): on the one host tried, `install` fetched and
verified the bootstrapper, the client installed and updated itself to build
1792618285, and applejack read it as `needs_login` and stopped it; but the
login window did not draw in two starts, and that is under investigation.
Readiness is inferred from a `steamwebhelper.exe` process plus the client
manifest; the window itself is not seen. Nothing from [Launch](#launch)
onward has run against a real client: the `RunningAppID` marker, the
`-applaunch` adoption, the `libraryfolders.vdf` edits and the client
restart they can need, the `Z:` fallback path, and the A2 rules have only
been exercised against the stub bundle, and are marked **unverified**
below.

## State

`applejack runtime steam status` and `runtime.steam.status` read the state
from disk every time; nothing caches it.

| State | Read from |
|---|---|
| `absent` | no `prefixes/steam-client/drive_c/Program Files (x86)/Steam/steam.exe` |
| `installing` | `applejack/client-install.json` in the prefix while another process holds the prefix lock (probed without taking it, so a status read neither locks nor settles anything) |
| `broken` | the prefix record does not open, `Steam/` exists without `steam.exe`, or the install marker is there and nobody holds the lock (an install died) |
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
| `runtime steam install` | `runtime.steam.install` (job) | Clears the marker of an install that died, builds the prefix if needed, downloads `SteamSetup.exe`, verifies it, runs it silently, lists the shared library in `libraryfolders.vdf`, starts the client for its self-update, and leaves it at the login window. With the client installed and up, only the wait for the window: it is the way to open the window for a sign-in |
| `runtime steam status [--json]` | `runtime.steam.status` | The table above |
| `runtime steam update` | `runtime.steam.update` (job) | Stops the client, snapshots the prefix, starts the client so it updates itself, records the build |
| `runtime steam repair` | `runtime.steam.repair` (job) | Refuses with `ClientRuntimeBroken` unless the runtime is `broken`. A dead install's marker is cleared first; with the client intact after that, nothing is restored. Otherwise stops the client and restores the newest usable snapshot (its prefix record opens and it holds `steam.exe`), reporting each newer snapshot it skips and why; with no usable snapshot it refuses and names `--reinstall`. The safety copy the restore takes of the state repaired away is discarded when that state had no `steam.exe`, and kept, named in the report, when it had one. Succeeds only when the runtime no longer reads `broken`. `applejack prefix restore steam-client <timestamp>` is the deliberate rollback that keeps a safety copy |
| `runtime steam repair --reinstall` | `reinstall: true` | Stops the client, removes `Steam/package/`, and runs the bootstrapper and first run again |
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
payload without the tool refuses with `ClientRuntimeBroken`.

The bootstrapper runs as `SteamSetup.exe /S` in the prefix, bounded by ten
minutes, and installs under `C:\Program Files (x86)\Steam`.

### First run and readiness

Before the client first starts, `config/libraryfolders.vdf` is written with
the client's own folder (`C:\Program Files (x86)\Steam`) and the shared
library (`S:\`), through a sibling file and a rename, so a client that has
never run already knows the library (**unverified** live: the client may
rewrite the file on its first run, in which case the first `run` adds the
folder again and restarts the client, below).

`steam.exe` is started with no arguments under a session key of its own,
recorded in `meta.steam_client_session`. The client downloads the rest of
itself from Valve. Readiness is read as: `package/steam_client_win64.manifest` (or `win32`)
exists, nothing is under `package/tmp/` for five looks in a row, and a
`steamwebhelper.exe` (the process that draws the window) is under the key,
or `config/loginusers.vdf` changed since the start. Ten minutes without it
stops the client with `ClientRuntimeBroken { state: updating }`; a client
that exits before that is `broken`. The client is left running for the
user to sign in; the CLI returns.

The login itself is the user's, inside the client window. applejack never
passes credentials to the client.

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
   its own files, not the title's. When no client is up, this `steam.exe`
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
