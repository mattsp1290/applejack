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

## State

`applejack runtime steam status` and `runtime.steam.status` read the state
from disk every time; nothing caches it.

| State | Read from |
|---|---|
| `absent` | no `prefixes/steam-client/drive_c/Program Files (x86)/Steam/steam.exe` |
| `installing` | `applejack/client-install.json` in the prefix while the prefix lock is held by a live process |
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
| `runtime steam install` | `runtime.steam.install` (job) | Builds the prefix if needed, downloads `SteamSetup.exe`, verifies it, runs it silently, starts the client for its self-update, and leaves it at the login window. With the client installed and up, only the wait for the window: it is the way to open the window for a sign-in |
| `runtime steam status [--json]` | `runtime.steam.status` | The table above |
| `runtime steam update` | `runtime.steam.update` (job) | Stops the client, snapshots the prefix, starts the client so it updates itself, records the build |
| `runtime steam repair` | `runtime.steam.repair` (job) | Stops the client and restores the newest snapshot of the prefix |
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
leaf signer's `O=` must begin with `Valve Corp`. Anything else is
`BootstrapperUnverified`, which names the signer found (or "no signature
found"); the file stays where it is for inspection and nothing runs. A
payload without the tool refuses with `ClientRuntimeBroken`.

The bootstrapper runs as `SteamSetup.exe /S` in the prefix, bounded by ten
minutes, and installs under `C:\Program Files (x86)\Steam`.

### First run and readiness

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
9); `repair` restores the newest snapshot. A run that finds the client
updating refuses with `ClientRuntimeBroken { state: updating }` rather than
snapshotting under it. `steam.cfg` with `BootStrapperInhibitAll=enable` is
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
   `Z:` plus the Unix path. The entry lists the appid with its
   `SizeOnDisk`. The file round-trips through the VDF code; a second launch
   changes nothing.
5. `steam.exe -applaunch <appid> -silent -noverifyfiles [launch_args_override]`
   is started in the prefix with this run's session key. `-silent` keeps
   the client's window down, `-noverifyfiles` skips the client's check of
   its own files, not the title's. When no client is up, this `steam.exe`
   becomes the client and `meta.steam_client_session` is set to the run's
   key; when one is up it hands off and exits. No flag came out of the
   CodeWeavers notes in `docs/research/`: they say only that a fresh
   bottle fixes `steamwebhelper` crashes. `-no-cef-sandbox` and
   `-noreactlogin` circulate as fixes and are **unverified**.
6. `game_config.steam_overlay` and `steam_input` are read and not applied
   yet. The client keeps both per game in `userdata/<id>/config/localconfig.vdf`;
   the key names are **unverified** and writing the wrong ones would do
   nothing quietly, so the defaults from plan 05 (overlay off and Steam
   Input off under D3DMetal) are a follow-up.

### Session marker

The game-running marker is `HKCU\Software\Valve\Steam\RunningAppID`, read
every tick with `<loader> reg query` in the prefix (never from `user.reg`,
which `wineserver` writes lazily). Equal to the appid, the title runs; `0`
or absent, it does not. When the value cannot be read, the fallback is a
process named after the title's launch exe under the run's key or the
client's.

Session start is when the marker first shows; the row goes `running`. The
client has `client_start_wait` (five minutes; it may verify files first) to
report it, else the watch ends with `ClientLaunchTimedOut`, the client is
stopped, and the row is `failed`. Session end is when the marker clears
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

### A2: the client re-downloading

Assumption A2 (plan 00) is that the client adopts the install with a
verification pass only. Every tick the watch reads the `.acf` and, every
five seconds, the install directory. Any of these refutes it:

- `buildid` differs from the backup;
- `StateFlags` has an update bit (`UPDATE_REQUIRED`, `UPDATE_RUNNING`,
  `UPDATE_STARTED`, `PREALLOCATING`, `DOWNLOADING`, `STAGING`, `COMMITTING`,
  `ADDING_FILES`; `VALIDATING` and `APP_RUNNING` are fine);
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
| First run never reached the window | `ClientRuntimeBroken { updating }` | The client was stopped; `install` again, or `repair` once a snapshot exists |
| Run with no client | `ClientRuntimeUnavailable { absent }` | `applejack runtime steam install` |
| Run with no account | `ClientRuntimeUnavailable { needs_login }` | `applejack runtime steam install`, sign in inside the window |
| Run while updating or broken | `ClientRuntimeBroken` | `status`, then `repair` |
| Client reported no running title | `ClientLaunchTimedOut` | The client was stopped; the session log names the run. A title the client refuses to launch (ownership, region) shows here |
| The client began downloading the title | `ClientRedownloadRefused` | A2 refuted; see above |
| Another title runs through the client | `PrefixBusy` on `steam-client` | Wait for it, or `app cancel` it |

## Recovery

- A client that will not start: `applejack runtime steam repair` restores
  the newest snapshot; `--reinstall` keeps the prefix and user data, drops
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
- The per-game overlay and Steam Input toggles in `localconfig.vdf`.
