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
| `broken` | the prefix record does not open, `Steam/` exists without `steam.exe`, the install marker is there and nobody holds the lock (an install died), a `bin/cef/cef.win*/steamwebhelper.exe` is the [web helper shim](#the-web-helper-shim) (by its marker) with no `steamwebhelper_real.exe` beside it, or with one that is itself a shim, or (read after the `updating` reads below, so a self-update still filling the tree is not taken for it) a `cef.win*` directory holds neither `steamwebhelper.exe` nor `steamwebhelper_real.exe`, or the installed client has no `cef.win*` directory at all, or a populated `cef.win*` directory holds the client's own `steamwebhelper.exe` in front, with no `steamwebhelper_real.exe` beside it (the client made the directory after the shim pass) or with the stash beside it (the client wrote its helper back over the shim), the reason naming which (the helper runs without the shim and draws no window, running client or not; `applejack runtime steam stop`, then `install`, or `repair` alone, put the shim in front of it without a download); a helper file that does not read is `broken` with the io error, never a failed `status` |
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
| `runtime steam install` | `runtime.steam.install` (job) | Clears the marker of an install that died, builds the prefix if needed, downloads `SteamSetup.exe`, verifies it, runs it silently, lists the shared library in `libraryfolders.vdf`, puts the web helper shim in place, starts the client (`steam.exe -noverifyfiles`) for its self-update, and leaves it at the login window; after every client start whose wait returns the window (or its fallback signal), the `cef.win*` directories are scanned again, and a helper laid down without the shim in front (the first self-update, a client update, or a directory the client made at that start: build 1792618285 made `cef.win64` beside `cef.win7` and `cef.win7x64` at its second start live) gets the client stopped and started once more with the helper stashed and the shim in front, at most twice per `install`, `update` or `repair`; a third finding ends the op `broken` naming the directory and its shape (the stash beside the helper or not), the reason `status` then reads. A start that shuts down on its own with nothing in `logs/bootstrap_log.txt` since the start but the shutdown (no download line; seen twice live) is started once more, once per `install`, `update` or `repair`, before the exit is an error. A shim with no helper beside it is removed first and that one start runs without `-noverifyfiles`, so the client's file check lays the helper down (**unverified** live); when that start ends without the helper laid down, the error is `ClientRuntimeBroken { broken }` naming `repair --reinstall`, not an update to wait for. With the client installed and up (its `wineserver` runs; a client the record names whose server is gone died and is started like one that is down: `install` stops nothing, since no server is there to stop, and `repair` prints `stop:` first), only the wait for the window: it is the way to open the window for a sign-in; a helper that is not the shim under a running client (one started outside applejack, or a self-update while it ran, wrote its own back over the shim; or the client made a directory the pass never saw) is `broken`: the `shim:` line names the shape and `stop`, then `install` or `repair`, and the op refuses with that reason instead of waiting for a window that cannot draw; a running client whose directory holds the orphan shim or no helper at all gets a `shim:` warning naming `stop`, then `install`, and the op then waits and fails `broken` with a detail that names `applejack runtime steam stop` first, then the reason's way out (`install` on a running client only waits again). Only a start that reaches the window (or its fallback signal) and is read by the state line prints one: a `broken` there (a helper that does not read after it, for one) ends the op with `ClientRuntimeBroken { broken }` after the `broken:` state line. Every other start failure ends it with `ClientRuntimeBroken` and no state line, after the last line the start printed: the restart limit after `client: <dir>/steamwebhelper.exe is still not the shim after 2 restarts; giving up`; a window timeout after `stop: stopping the Steam client`, carrying `status`'s detail under the timeout's lead line (with the timeout's own way out, `install` again, added when that reading names none); a client that exits during the wait (the stall, a file-check start that laid no helper down, the early exit) after its last `client:` line, since every `start:` line is followed by at least one (`client: starting`, or `client: updating`); a payload with no shim before any `start:` line. The error table below has each. Either way, an op that starts the client, or `install`'s wait on a client already up, never succeeds on a `broken` runtime; the CLI exits 1 |
| `runtime steam status [--json]` | `runtime.steam.status` | The table above |
| `runtime steam update` | `runtime.steam.update` (job) | Stops the client, snapshots the prefix, starts the client so it updates itself, records the build; a start that leaves the runtime `broken` ends the op with `ClientRuntimeBroken`, after the `broken:` state line when the start reached the window and after its last `client:` or `stop:` line when it did not (before any `start:` line for a payload with no shim), as `install` does |
| `runtime steam repair` | `runtime.steam.repair` (job) | Refuses with `ClientRuntimeBroken` unless the runtime is `broken`. A dead install's marker is cleared first; with the client intact after that, nothing is restored. A populated `cef.win*` directory with the client's own helper in front (the stash beside it or not), anywhere under `bin/cef`, on a client whose prefix record opens and which has `steam.exe` (the breaks `status` reads first; with one of those, or any `steamwebhelper.exe` or `steamwebhelper_real.exe` under any `cef.win*` directory that is not a regular file or does not open, the restore below runs, whatever `bin/cef` holds), is fixed in place, as `install` fixes it: `repair` stops the client if it runs, stashes the helper, puts the shim in front and starts the client to its window, downloading nothing and restoring nothing (`repair: <dir>: the client's own web helper is in front (<shape>); nothing to restore: ...`). Whatever else `bin/cef` holds beside that directory goes with the pass, as under `install`: an orphan shim is removed and the file-check start lays its helper down (**unverified** live); a populated directory with no helper is what the pass's end check reports (`broken:` naming it, and the op is `ClientRuntimeBroken`); so the `repair:` line may name a directory `status` did not. Otherwise `repair: <the break>; restoring the newest usable snapshot` names why (the break alone, without the way out the `status` detail gives), and `repair` stops the client and restores the newest usable snapshot (its prefix record opens, it holds `steam.exe`, and every `cef.win*` directory holds a web helper that is neither an orphan shim nor the client's own helper without the shim in front), reporting each newer snapshot it skips and why; with no usable snapshot it refuses and names `--reinstall`. The safety copy the restore takes of the state repaired away is discarded when that state had no `steam.exe`, and kept, named in the report, when it had one. Succeeds only when the runtime no longer reads `broken`. `applejack prefix restore steam-client <timestamp>` is the deliberate rollback that keeps a safety copy |
| `runtime steam repair --reinstall` | `reinstall: true` | Checks the payload first: `probes/osslsigncode` and the shim (the loader, `wineserver` and the bundle's files are checked when the engine loads, before any op). A missing one refuses with nothing stopped or moved and `status` unchanged. Then it stops the client, moves `Steam/package/` aside to `Steam/package.reinstall-old/` (`repair: moving <package> aside to <aside> until the new client reaches its window`), fetches `SteamSetup.exe`, and runs the bootstrapper and first run again; the bootstrapper lays no `bin/cef/`, so a shim without its helper is removed at that start (or a directory with no helper is left for the self-update) and the self-update, which the missing `package/` triggers, lays the helper down, which the restart stashes (**unverified** live: stub only; the self-update fetching the CEF tree is the mechanism the live `bootstrap_log.txt` shows, the restart after it is not). Once the new client has reached its window the old package is removed (`repair: removing <aside>`) and the end check runs as under `install`: a `broken` runtime fails the op, never success; the CLI exits 1. **A failed `--reinstall` leaves the old package in place**: any failure before that point (a download, a signature, a prefix the launch refuses, a shim that does not read, the bootstrapper, the first run, its timeout) stops the client if it runs and puts the old package back in place of whatever the run left (`package: restoring <package> from <aside>, since the reinstall failed`), so `status` then reads what it read before. A `package.reinstall-old/` left by a reinstall that died is restored the same way by the next `install`, `update`, `repair` or `stop` (`package: ..., left by a reinstall that did not finish`): the old package is the one known to be whole, and in the one window where the new one is (after its end check, before the removal) the restore costs that client one self-update |
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
the `shim:` line says the self-update brings it); `install`, `update`
and `repair` scan the `cef.win*` directories again after every start
whose wait returns the window (below). A `run`'s start is not scanned for
a restart (a restart there would end the game): once the title is up, a
directory the client made at that start gets one line (`client: the
client laid its web helper down without the shim in front of it at this
start (<dir>); the game runs on, but the client's window cannot draw and
`status` reads broken: after the game, `applejack runtime steam stop`,
then `applejack runtime steam install` or `applejack runtime steam
repair`, put the shim in front of it`), the game runs on, and the next
`status` or `run` reads that directory `broken` until the shim pass. Each
start then prints that macOS
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
`logs/bootstrap_log.txt`, except once per `install`, `update` or `repair`, at
whichever of its starts does it first: when the lines `bootstrap_log.txt`
gained since the start carry `Shutdown` and no line containing `download`
(in any case) (the client ended on its own before its version check; the
live first start did so twice, `Verification skipped`, `Shutdown` 16 s
later, nothing else, and the next start was fine), the client is started
once more (`client: the client shut down on its own with nothing but the
shutdown in logs/bootstrap_log.txt (no download line); starting it once
more`), and a second such exit in the same op is the error. The rule is
the one shape seen live and nothing wider, decided so: a `Shutdown` after
a download line (`Download skipped`, `Downloading ...`, an update fetched)
is not retried, since the client got past its version check and ended
after it, which is not the early self-shutdown and may be a failure of
its own the user should see; an exit with no `Shutdown` line is not
retried either, since the client did not end itself, it died, and the
error names the log. Whether a prefix that has started before logs a line
with the word `download` at every start (a cached download-hosts line)
is **unverified** live; if it does, the retry fires on fresh prefixes
only and such an exit falls through to the error, never to a loop.

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
(**unverified** live: stub only). When that start ends with the directory
still without a helper, the error is `ClientRuntimeBroken { broken }`
saying so and naming `applejack runtime steam repair --reinstall` (at the
window timeout with the client's manifest present, as `status`'s reading
of the directory, under the timeout's lead line; with no manifest the
timeout reads `updating`; see below): with `package/` moved aside the
client's self-update fetches its CEF tree again, the one mechanism seen to
lay the helper down live. `status` reads that prefix as `broken` (no web
helper) and a run is refused until it is recovered.

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
presumably black as the real helper's window was in unit U15b, and closes
by itself); after a client update it writes its helper over the shim.
Either way, once the wait returns with the window (or its fallback
signal), every populated `cef.win*` directory is scanned again (a wait
that ends in the retry above or in an error never reaches the scan; the
next start's pass stashes anyway), and when one holds a
`steamwebhelper.exe` that is not the shim, the stash beside it or not,
the client is stopped (`client:
the client laid its web helper down without the shim in front of it
(<dir>); restarting the client with the shim in front of it (restart 1 of
2)`, `stop:`), the next start's shim pass stashes it (`shim:`), and the
client starts once more, so the window that is left on screen is one that
draws. The scan is not only for the helper the pass covered: the client
makes directories as it pleases, and build 1792618285 made `cef.win64`
beside `cef.win7` and `cef.win7x64` at its second start, after the pass had
shimmed the two, so that restart ran the new helper unshimmed with no window
(the live finding of 2026-11-06). At most two such restarts per `install`,
`update` or `repair`; a third finding prints `client: <dir>/steamwebhelper.exe is
still not the shim after 2 restarts; giving up` and the op ends
`ClientRuntimeBroken { broken }` naming the directory and the shape it
hit (`with steamwebhelper_real.exe stashed beside it`: the client wrote
its helper back at every start; `with no steamwebhelper_real.exe beside
it`: it made a directory the pass had not covered), the same reason
`status` then reads, the client left running for `stop`, then `install`
or `repair`. The bound is two because a fresh install uses both rounds
(the helpers the first self-update lays down with nothing stashed, which
the pass after that restart reports as `Applied`, `shim: <dir>: stashed
the web helper as steamwebhelper_real.exe and put the shim in front of
it`, then `cef.win64`) and no
real client has been seen to need a third; one that does ends there, and
`stop`, then `install`, shims everything it laid down in one start. A
window timeout (ten minutes without the window) stops the client and
fails with `ClientRuntimeBroken` carrying what `status` then reads, its
state and its detail, under the timeout's lead line `the client did not
reach the login window within 600 seconds; it was stopped and its files
kept`. The error and `status` cannot disagree (the rule for every op's
error, under Failure catalogue). Where `status` names a way
out, the error's is the same: `install` again for `needs_login`, and for
`updating` (it resumes the self-update), and the reason's own for
`broken`. Where it names none (`ready`, `absent`), the error adds the
timeout's own: `` `applejack runtime steam install` starts the client to
its window again``. `repair` refuses anything but `broken`. After a
file-check start, `install` repeats that start; a second identical
timeout after it means `repair --reinstall`. Otherwise the client is left running for
the user to sign in; the CLI returns.

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
pinned llvm-mingw). Before every client start, and again after every start
`install`, `update` and `repair` make whose wait returns the window (the
client's own tree decides which
`cef.win*` directories exist: `cef.win7`, `cef.win7x64` and, since build
1792618285 at the second start, `cef.win64`, the one the 64-bit client
runs), for every `bin/cef/cef.win*/` directory under the client's `Steam/`
that holds a `steamwebhelper.exe`:

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
  holds the orphan, or no helper at all, is skipped), and with the client
  stopped `install` is the recovery (with it running, `stop` first:
  `install` on a running client only waits): the orphan is removed (`shim: ...: removed an orphan shim; the
  client's file check lays its helper down at this start`), that one start
  runs without `-noverifyfiles` so the client's file check lays the helper
  down, and the restart after the window stashes it and puts the shim back
  (**unverified** live: stub only; a file check that lays nothing down
  ends in `ClientRuntimeBroken { broken }` naming `repair --reinstall`, and
  the directory then reads `broken` with neither helper file). `repair
  --reinstall` ends the same way: the bootstrapper lays no `bin/cef/`, the
  orphan is removed at the start (`...; the client's self-update lays its
  helper down`), and the self-update that the moved-aside `package/` triggers
  lays the helper down (**unverified** live: stub only);
- a `cef.win*` directory with neither `steamwebhelper.exe` nor
  `steamwebhelper_real.exe` (an installed client with no `cef.win*`
  directory counts) is `broken` the same way, with neither file to run:
  with the client stopped, `install` tries the file check, `repair
  --reinstall` the self-update; a
  game run never starts a client in that state, and one whose shim pass
  finds it (the directory emptied after `plan_run` answered) fails with the
  same reason;
- a populated `cef.win*` directory holding the client's own helper (not a
  shim by its marker) in front is `broken` for `status`, running client or
  not, in either shape: with no `steamwebhelper_real.exe` beside it (the
  client made the directory after the shim pass; `<dir>/steamwebhelper.exe
  is the client's own web helper with no steamwebhelper_real.exe beside
  it, ...`) or with the stash beside it (the client wrote its helper back
  over the shim: a self-update while it ran, or a client started outside
  applejack; `... with steamwebhelper_real.exe stashed beside it, ...`).
  Either way the helper in front runs without the shim and draws no
  window. `install` with the client stopped is the recovery (the pass
  stashes it, `Applied` or `Reapplied`; nothing is downloaded), and so is
  `repair`, which stops the client itself if it runs and then does the
  same pass; `install` with the client running says so under `shim:`,
  naming the shape, and refuses with the same reason instead of waiting
  for a window that cannot come;
- a stash with no `steamwebhelper.exe` in front of it gets the shim
  written back (`Rewritten`), with no file check;
- a payload without the shim refuses with `ClientRuntimeBroken` before the
  bootstrapper download and before any client start, since the window
  would not draw.

Each directory is reported under `shim:`. The check is by hash and marker,
never by name; a file over 1 MB is read as the client's helper without
hashing it (a shim build is about 68 KB, the helper several MB), so a
`status` read hashes nothing large. The shim resolves `steamwebhelper_real.exe` beside itself,
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
`WINEDEBUG` or `DXMT_LOG_LEVEL` goes through `prefix exec`:

```sh
applejack prefix exec steam-client --env WINEDEBUG=+seh --env DXMT_LOG_LEVEL=debug -- 'C:\Program Files (x86)\Steam\steam.exe' -noverifyfiles
```

(the launch's working directory is the prefix root, so the exe is named by
its Windows path); the pairs are the launch's game tier and anything
outside the allowlist is dropped and logged.

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

Every `ClientRuntimeBroken` that `install`, `update`, `repair` or `repair
--reinstall` fails with, a window timeout included, carries as its state
what `status` reads when the op returns (after any stop or restore the op
made); its detail is the failure's own, and the advice the message ends
with is that state's. A failed `repair --reinstall` leaves the old
`package/` in place, so `status` then reads what it read before the op.
The rows below name the failure and its way out.

| Failure | Code | What to do |
|---|---|---|
| Bootstrapper download failed | `DownloadFailed` | Check the network; nothing was written outside `applejack/SteamSetup.exe.partial` |
| Bootstrapper signature invalid or not Valve's | `BootstrapperUnverified` | Inspect the kept file with `osslsigncode verify`; the message names the signer |
| Payload without `probes/osslsigncode` | `ClientRuntimeBroken` | Refused before the download and, on `repair --reinstall`, before the stop and the move aside of `package/`. Install a payload that ships the tool (the message says so) |
| `SteamSetup.exe` failed or ran over ten minutes | `ClientRuntimeBroken` | The session log under `logs/` has the output; `repair --reinstall` tries again |
| First run never reached the window | `ClientRuntimeBroken` carrying `status`'s detail under the lead `the client did not reach the login window within 600 seconds; it was stopped and its files kept`, after `stop: stopping the Steam client` | The client was stopped and its files kept. The detail is `status`'s, and so is its way out where `status` names one: `install` again for `needs_login` and `updating` (it resumes the self-update), the reason's own for `broken`. For `ready` and `absent`, which name none, the error adds `` `applejack runtime steam install` starts the client to its window again``. `repair` refuses anything but `broken`. A second identical timeout after a file-check start's `install` means `repair --reinstall` |
| The start without `-noverifyfiles` on `install`, with the client's manifest present, ended with the client exiting and no helper laid down | `ClientRuntimeBroken { broken }` | The directory held no helper going in (an orphan shim was removed, or none was there) and holds neither `steamwebhelper.exe` nor `steamwebhelper_real.exe` now; `install` once more is named first (the file check is cheap and may succeed), then `repair --reinstall`, which replaces `package/` so the self-update fetches the CEF tree (the mechanism seen live). At the window timeout instead, the error is the first-run row's: `status`'s reading of the directory, the next row |
| A `cef.win*` directory holds neither `steamwebhelper.exe` nor `steamwebhelper_real.exe`, or the installed client has no such directory | `ClientRuntimeBroken { broken }` | With the client stopped, `install` starts the client with its file check on (**unverified** live), `repair --reinstall` through the self-update; a run is refused, and a run whose shim pass finds it fails with the same reason. An interrupted self-update (`package/tmp` present, no client running, the old manifest in place) reads this way too; `install` resumes it. A directory with no entries at all beside one that holds the helper is skipped, not read as this (`install` says so under `shim:`) |
| The client died about 3 minutes in with `BMainLoop appears to have stalled` in the session's `wine.log`, `logs/console_log.txt` or `logs/bootstrap_log.txt`, or with a fresh `assert_steam.exe_*.dmp` under `dumps/` | `ClientRuntimeBroken` | The macOS microphone prompt for "applejack" was left unanswered (with a dump alone, the usual cause); answer it (Don't Allow is fine; the answer is stored) and `install` again. `status` does not show the stall: the client is simply gone |
| `steamwebhelper.exe` is the shim and `steamwebhelper_real.exe` is missing beside it (or is a shim too) | `ClientRuntimeBroken { broken }` | With the client stopped (`applejack runtime steam stop` first if it runs; the error says so when `install` found it up), `install` removes the orphan shim, starts the client once with its file check on so it lays the helper down, and restarts it with the helper stashed and the shim in front (**unverified** live: stub only); `repair` restores a snapshot holding helper and stash when one exists (unless a populated directory beside it holds the client's own helper in front: then the in-place pass, which removes the orphan and runs the file-check start, as `install` does); `repair --reinstall` also recovers, through the full self-update (**unverified** live) |
| Payload without `probes/steamwebhelper-shim.exe` | `ClientRuntimeBroken` | Install a payload that ships the shim; without it the login window does not draw. Refused before any start: on an `install` that runs the bootstrapper, by the payload check before the download (after `prefix: creating prefix steam-client` on a fresh one); on `repair --reinstall`, by the same check before the stop and the move aside of `package/`; on an installed client's `install` or `update`, by the shim pass, before any line of its own. `status` does not show the missing shim |
| A populated `cef.win*` directory holds the client's own `steamwebhelper.exe` in front, with no `steamwebhelper_real.exe` beside it (the client made the directory after the shim pass) or with the stash beside it (the client wrote its helper back over the shim) | `ClientRuntimeBroken { broken }`, and `status` reads `broken` naming the directory and the shape | The helper runs unshimmed and draws no window (live: `cef.win64` made at the client's second start, `EGL_BAD_ALLOC` in `cef_log.txt`, `running yes` and no window). `applejack runtime steam stop`, then `install`, or `repair` alone (it stops the client itself): the pass stashes the helper and puts the shim in front, with no download and no snapshot restore. `install` under the running client says so under `shim:` and refuses with this reason |
| `install`, `update` or `repair`'s in-place pass found a helper without the shim after each of its two restarts | `ClientRuntimeBroken { broken }` naming the directory and the shape, after `client: <dir>/steamwebhelper.exe is still not the shim after 2 restarts; giving up` | The client writes its helper back over the shim at every start (the stash beside it) or makes a new directory at every start (no stash); the client is left running. `stop`, then `install` or `repair`, shims what is there; if the same directory keeps coming back, report it with the `shim:` and `client:` lines |
| A `run` started the client and the client made a `cef.win*` directory at that start | none for the run (`client: the client laid its web helper down without the shim in front of it at this start (<dir>); ...` once the title is up); the next `status` or `run` reads `broken` naming the directory | The game runs on and the client's window cannot draw; a restart there would end the game. After the game, `stop`, then `install` or `repair`, put the shim in front |
| A start shut down on its own (`Verification skipped`, `Shutdown`, no download line in `logs/bootstrap_log.txt`) | none the first time in the op (`client: the client shut down on its own ...; starting it once more`), `ClientRuntimeBroken` (the early exit) on the second | Seen twice live on a fresh prefix, the next start fine both times; cause unknown. A `Shutdown` after a download line, or an exit with no `Shutdown` line, is not retried (above). After the error, `install` again |
| The window is on screen but black and stays after `install` returns, or no window is on screen with `status` reading `running yes` | none, or `broken` when `status` reads the unshimmed helper | The helper ran without the shim: check `shim:` lines in the `install` output and `logs/webhelper.txt` for `steamwebhelper_real.exe` in the `webhelper launched` line (a `webhelper launched` line naming `steamwebhelper.exe` under a directory no `shim:` line named is the unshimmed case), and `cef_log.txt` for `EGL_BAD_ALLOC`. A black window that closes by itself during a fresh install is the real helper's, before the restart with the shim |
| An install died and left its marker | `ClientRuntimeBroken { broken }` | `install` again: it holds the lock, so the marker is a dead install's and is cleared first. `repair` clears the marker and, with the client intact, stops there |
| `repair` found no usable snapshot | `ClientRuntimeBroken { broken }` | None listed, or every one skipped: a prefix record that does not open, no `steam.exe`, no web helper beside the shim, the client's own helper without the shim in front, or a helper that does not read (each is reported as skipped with its reason; `status` shows the count, not which are usable); `repair --reinstall` reruns the bootstrapper |
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
  dead install's marker and, unless the client is intact after that, puts
  the shim back in front of a client helper that lost it (no restore) or
  restores the newest usable snapshot of a `broken` client; it refuses any
  other state and fails rather than leave the runtime `broken`;
  `--reinstall` keeps the prefix and user data, replaces
  `Steam/package/` (the old one stays aside until the new client reaches
  its window, and comes back if the reinstall fails), and reruns the
  bootstrapper. The CodeWeavers note that
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
