# Steam layer

`crates/applejack-steam` owns authentication, refresh-token storage, the
owned-app library, Windows depot install, update and verify, `.acf` and `.vdf`
handling, and the advisory locks that serialize applejack processes. It
depends on external crates only, never on `applejack-config`,
`applejack-engine`, or another applejack crate; it accepts and returns plain
structs.

Gate G2 status: **green** (2026-10-30, `steamroom` and `steamroom-client`
0.3.0, macOS 26.6.2, `--credentials` with a Steam Guard device code). Login,
refresh logon, and the license list all worked against live Steam, and the CM
asked for a 9 s heartbeat. The Rust path stays primary. QR mode was not run.

Approving the mobile prompt and then typing a code answered EResult
`DuplicateRequest` at G2. SteamKit treats that as success because the next
poll returns tokens; `steamroom-client`'s `GuardChallenge` treats it as fatal
and keeps its poll loop private. Every call it makes is public on `steamroom`'s
`SteamClient`, though, so `live::SteamroomConnection` holds the auth session
itself and needs no fork: one connection polls between code submissions, and
`DuplicateRequest` at the submit means the tokens are on the next poll. Either
path finishes a login, whichever comes first. This has passed a scripted CM
(`tests/live_session.rs`); it has not yet run against live Steam.

## Flows

All flows go through `auth::Authenticator`, which drives a `SteamConnection`
and a `RefreshTokenStore`. `SteamConnection` covers the auth flow only;
`logon` returns a `SteamSession`, described under "Session". It tries up to
three TCP CMs in turn, moving on when one will not connect, answers
`TryAnotherCM` (`EResult` 48), hangs up before answering the logon, or does not
answer it in 30 s; a refusal, a rate limit, a Guard error, or
`CLIENT_LOGGED_OFF` ends it there. Unit tests use scripted fakes; `live::SteamroomConnection` and `live::SteamroomSession` are
the only implementations that open a socket.

- **Polling.** `Authenticator::poll` is one look at the pending session and is
  valid while a code is awaited. Each answer is a `LoginStep`: `QrReady` with a
  rotated challenge, `GuardRequired` while a code is still wanted, `Waiting`, or
  `Done`, by which point the refresh token is stored. `finish()` polls at
  Steam's interval for a driver with no code to wait for. `LoginState` lets a
  login be put down between RPC requests and picked up with
  `Authenticator::resume`. `core::steam::LoginFlow` is the one driver the CLI
  and `steam.login.*` share.
- **QR login.** `begin(AuthMode::Qr)` calls `BeginAuthSessionViaQR` and returns
  `LoginStep::QrReady { payload }`; the shell renders the payload as a QR code.
  Polling follows `new_client_id` and `new_challenge_url`, which `steamroom`'s
  own `poll_auth_session` drops, so a rotated challenge reaches the user. QR
  mode has not run against live Steam.
- **Credentials login.** `begin(AuthMode::Credentials, Some(credentials))`
  fetches the account's RSA key, sends the encrypted password to
  `BeginAuthSessionViaCredentials`, and returns either `Waiting` (no second
  factor, so polling finishes it), `GuardRequired { factor: EmailCode | DeviceCode }` (poll, and call
  `submit_guard_code` when a code arrives), or `GuardRequired { factor:
  DeviceConfirmation }` (poll). `submit_guard_code` polls once, so `Waiting`
  from it means the code was taken. `LoginFlow` allows `Waiting` two minutes and
  then ends the login with `SteamLoginStalled`, code `SteamLoginFailed`; the CLI
  prints a line while it waits. An answer with an empty refresh token is not an
  approval: it would store nothing usable and read as signed in.
  `InvalidGuardCode` keeps the session open.
  Any other error from `submit_guard_code` or `poll` ends the login: the
  connection has dropped its auth session, the driver returns to idle, and the
  next call must be `begin`; `poll` or `submit_guard_code` before that is
  `OutOfOrder`, never `GuardRequired`, so a shell that re-prompts on
  `GuardRequired` cannot loop. A call in the wrong state is `OutOfOrder` and
  leaves the connection's state unchanged.
- **Refresh.** `refresh(account_name)` reads the stored refresh token, sends
  `CMsgClientLogon` with it on a fresh connection, and returns the session.
  The account name is not a secret; the token logon requires it alongside
  the token, so the host persists it next to its other metadata (the `meta`
  table, key `steam_account_name`). `AuthError::NoStoredToken` means the user
  must log in; `AuthError::Refused` means the token was rejected. Any failed
  silent refresh exits the CLI with 3, and only those two answer
  `SteamLoginRequired` on the wire; the rest keep their own code, since a login
  would not fix a network. Only a login answers `Refused`. A call denied on a healthy session
  is `AuthError::CallDenied { call, result }`, and sending that user back to
  log in would not help.
- **Logout.** `logout()` deletes the Keychain item. `Core::steam_logout` also
  clears the library cache and the account name, in one transaction; titles on
  disk stay, with their `drm` and launch exe. It is local only: the token is not
  revoked at Steam, which still lists the device until the user removes it from
  their account. A refresh that outlasts a logout, or a login to another
  account, drops its rows and fails with `SteamLoginRequired`: the fetch writes
  through `cache_library_for`, which checks the account in the same transaction.

Rate limits and Steam Guard surface as typed errors (`AuthError::RateLimited
{ retry_after }`, `AuthError::GuardRequired { factor }`); nothing loops on
them. The CM does not report a retry-after, so the value is a fixed 60 s floor.

## Session

`session::SteamSession` is everything after logon, and every method takes
`&self`, so a caller can hold one session across concurrent calls.
`live::SteamroomSession` implements it around one receive-and-dispatch loop.

- **One receive loop.** A spawned task owns `recv_msg` for the life of the
  session and routes each message. A request registers a waiter under a fresh
  job id and only sends; replies come back by `jobid_target`. `steamroom`'s own
  calls each run a private receive loop and would consume each other's
  replies, so none of them is used on a session. A reply without a job id goes
  to the oldest request expecting that message, except a service reply, which
  always echoes its job; a reply for a job that timed out is dropped, so a
  timeout costs one call, not the session.
- **`Ready`, not `LoggedIn`.** `SteamClient<Ready>::login` returns on the
  logon response and discards the other messages in its `MULTI`, and
  `LoggedIn` offers only the private-loop calls. `Ready` has `send_msg` and
  `recv_msg`, which is all the loop needs, so the session sends `CLIENT_LOGON`
  itself after the loop starts and stays in `Ready`. The license list is no
  longer racing the logon: it is caught whether it shares that `MULTI` or
  arrives later. The cost is that `live/session.rs` carries the wire form of
  each PICS, depot-key, and content-server call, built from the generated
  protos.
- **License list.** The loop keeps the newest `CMsgClientLicenseList`, which
  Steam sends unprompted after logon and on any change. `license_list` answers
  from it and waits up to 30 s for the first. Call it as often as needed.
- **Heartbeat.** A second task sends `CLIENT_HEART_BEAT` at the CM's interval
  (9 s when it does not say) whether or not a call is waiting. No caller
  schedules anything.
- **Failure.** A transport error, `CLIENT_LOGGED_OFF`, or an undecodable
  `MULTI` closes the session: every waiter and later call gets that error, and
  the caller must `logon` again. Dropping the session aborts both tasks and
  closes the socket. PICS replies split with `response_pending` are collected
  before the call returns. Each reply is waited for up to 60 s.
- **CDN.** `live::LiveChunks` fetches chunks and manifests over `steamroom`'s
  `CdnClient` with a 120 s bound per request. It replaces
  `steamroom_client::download::CdnChunkFetcher` because a CDN auth token sits
  in the URL and `reqwest::Error` prints the URL, so HTTP errors leave through
  `without_url`; and because a token is per depot and host, requested when a
  host answers 403, whether it never had one or its token expired, and at most
  once in 30 s per depot and host. Server entries follow `SteamKit` rather than
  `steamroom`'s `get_cdn_servers`: requests go to `vhost`, over HTTPS only when
  `https_support` is `mandatory`, on port 443 or 80, and the listed `host` is
  used only to name the server to `GetCDNAuthToken`. A manifest fetch makes one
  pass over the pool: a 429 or 503 cools that server and moves on, and the rate
  limit is the answer only if no server had the manifest. A 404 from any server
  with none serving it reads as no longer served. The pool's cooldown is the
  only wait, and nothing loops.

## Library

`library::Library::fetch` walks licenses to packages (`package_grants`, a binary
`KeyValues` buffer behind a 4-byte prefix) to appids to PICS product info
(`app_info`, text `KeyValues`, access tokens included). Keys match in any case,
as under `SteamKit`'s indexer. `Library` returns plain
values; the caller persists them to the `apps` table with `fetched_at`.

`OwnedApp { appid, name, installdir, size, default_branch, depots, drm,
platforms, builds, change_number, free_to_download }` describes one app. `drm` is always
`unknown`: it cannot be inferred from Steam, and the config database overrides
it. `Library::windows_apps` is the installable list.
`Library::windows_app(appid)` answers `AppNotOwned`, or `NoWindowsDepot` naming
the platforms the app lists.

`OwnedApp::windows_depots(branch, filter)` picks what to fetch: depots whose
`oslist` includes `windows` or is empty, on an app whose own `oslist` allows
Windows; `osarch` 64 and neutral, or 32 when the app ships no 64-bit depot of
its own (a DLC depot does not count); the filter's language (`english` by
default) or none; no `lowviolence` depots. A depot with no manifest on a branch
the app has takes its `public` one, as under `DepotDownloader`, and its request
code then names `public`. A branch with a non-zero `pwdrequired`, or one any
depot lists under `encryptedmanifests`, is left out of `builds`, so it is refused
with `NoManifestForBranch` rather than installing `public` under its build id.

What the account may fetch follows `DepotDownloader`'s `AccountHasAccess`:
`package_grants` reads both `appids` and `depotids` from every licensed
package, and a depot is available when either lists it, when its `dlcappid` is
owned, or when the app is `freetodownload` and the depot is not paid DLC. The
rest are left out and listed in `DepotSelection::not_licensed` and
`InstallPlan::not_licensed`, and a title with none left is `NoLicensedDepot`.
`DepotFilter::default()` licenses nothing, so build the filter with
`Library::filter`.

A `depotfromapp` depot with no manifests of its own lives in another app, often
one the account does not own, such as 228980 for redistributables. `Library`
reads that app's product info and takes the depot's manifests from it, one
level. Its manifest request and CDN token then name the parent, unless the
title is free to download; the
depot key still names the title. Such a depot installs into the title's own
directory, as under `DepotDownloader`, where the Windows client would use the
parent's shared directory and list it under `SharedDepots`. A licensed one
that cannot be resolved refuses the install with `UnresolvedSharedDepot` naming
both ids, so `StateFlags` 4 never covers missing content.

## Install, update, verify

`depot::InstallPlan::new(app, branch, filter)` chooses the depots and refuses
an `installdir` that is not one plain name. Build it from fresh product info:
Steam keeps serving old manifests, so a stale plan installs a stale build.
`depot::DepotInstaller::new(session, paths, data_dir, client_running)` then
runs it. `steamroom-client`'s `DepotJob` does the file work, through the
`ChunkFetcher` trait; the installer decides what `DepotJob` sees by handing it
a manifest trimmed to the files that still need writing.

```text
<library>/steamapps/common/<installdir>/                 the game
<library>/steamapps/appmanifest_<appid>.acf              written last
<library>/applejack/<appid>.download.json                exists only mid-download
<library>/applejack/manifests/<depot>_<manifest>.json    decrypted file lists
```

- **`install(&plan)`** takes the per-appid lock, then the shared library lock,
  both non-blocking, so a held lock fails at once with `Error::LockHeld` naming
  the holder's pid and command. It refuses a title whose `.acf` is present:
  `AlreadyInstalled` when fully installed, `ClientOwned` otherwise.
- **Verification.** `DepotJob` checks each chunk's Adler-32 and SHA-1 id,
  assembles the file under `<installdir>/.DepotDownloader/staging`, renames it
  into place after a whole-file SHA-1, and is removed from the install on
  success. A corrupt chunk is `Error::CorruptChunk`, never reaches the
  install, and leaves the record for the next run. `steamroom` does not check
  a manifest's signature, so the installer refuses any file name or symlink
  target that would leave the install directory (`Error::UnsafePath`), for a
  stored manifest as for a fetched one. A symlink already on disk is checked
  too, since `DepotJob` joins paths and follows them: before the first write,
  every parent of a queued path and of staging is walked, and one that leaves
  the install fails with `Error::LinkOutOfInstall` naming the link and target.
  Stale removal skips such a path. A link that stays inside is fine. The
  install directory itself may be a symlink on an installed title, which is
  how a game moves to another disk, and not on a title with no `.acf`.
- **Resume.** `<appid>.download.json` (`DownloadRecord`) lists, per depot, the
  target manifest, the previous manifest, and the files in place. It is saved
  atomically every 256 files and on every stop. A run whose branch and
  manifests match resumes: recorded files whose size still matches are
  skipped, a half-assembled file reuses its staged chunks after re-hashing
  them, and any file finished after the last save is hashed rather than
  fetched. A record for other manifests is replaced, and because that run may
  have left its own files, the new record is marked `rescan`: the whole
  manifest is hashed and anything that differs is rewritten, across stops,
  until the run finishes, and an unreadable record counts as naming every
  depot. The record outlives
  `Error::DiskFull`, a CDN failure, and a client starting; it goes only after
  the `.acf` is written.
- **`update(&plan)`** is `--update`. It re-reads the `.acf` under the lock,
  refuses anything but fully installed with `ClientOwned` (which names
  `applejack runtime steam`), and diffs each depot from the manifest the `.acf`
  records, fetching that manifest if it is not stored, as for a title the
  client updated. Only files that differ are written, unchanged chunks of
  those files come off disk, and files gone from the new manifest are removed
  unless another installed depot still owns them. A path is not removed while
  it is the same file (device and inode) as a current one: APFS folds case and
  Unicode form, so a respelled file is one file, and the volume is the only
  thing that knows its own rules. A stale entry in the way of a queued path, a
  file that became a directory or the reverse, goes before the fetch. Removal
  is skipped altogether when an
  untouched depot's manifest is not stored, because nothing can then be proved
  stale. A depot already current is left alone, unless a stopped run's record
  names it: such a depot is rechecked in full whatever the `.acf` says, since
  the `.acf` is what the stopped run did not move. A recorded depot the plan
  no longer selects goes back to its `.acf` manifest, or is refused with
  `Error::UnfinishedDepot` when the `.acf` does not list it, and the record
  stays. A record that cannot be read counts as naming every depot the `.acf`
  lists. Files only an abandoned target wrote are removed as stale, and a path
  an abandoned target wrote is rewritten by whichever depot owns it now. A
  manifest says what a stopped run meant to write, not what it wrote, so an
  entry counts as written only while the file on disk matches it: anything else
  at that path was never reached or has been edited since, and stays. The rewrite
  keeps every `.acf` key the writer does not model, and keeps depots the plan
  did not select. The caller re-syncs its `apps` row from the `.acf` first and
  afterwards.
- **Overlapping depots.** A path is on disk once, and the depot with the
  highest id wins; `path_owners` is the one place that says so, for what to
  write and for verify. Numeric order is a choice: `KeyValue` keeps children in
  a map, so the buffer's own order is lost. A delta queues a path only for its
  owner, so it never overwrites a later depot's overlay, and a depot that drops
  an overlay brings the lower depot's file back, even when that depot did not
  change. One gap remains: a depot the plan does not select and whose manifest
  is not stored, such as a language the client installed, has unknown paths, so
  a lower depot can overwrite its overlay. Refusing would block every update of
  such a title.
- **Config files.** A manifest entry flagged `USER_CONFIG` (`userconfig` in a
  depot build script: "modified by the user or game", not overridden by an
  update, no verification error when it differs) is installed once and never
  overwritten, deleted as stale, or reported by verify. A
  `VERSIONED_USER_CONFIG` entry behaves the same except that it is replaced
  when the depot's own copy changes. Either is reported as missing, and put
  back by a run that reaches it, when it is gone from disk.
- **`verify_install(paths, data_dir, appid)`** is `--verify`. It takes the same
  locks, needs no session, re-hashes every file against the stored manifests of
  the depots the `.acf` records, and returns `VerifyReport { files_checked,
  mismatched, missing }`. Files the manifests do not list are ignored. A depot
  this crate did not install has no stored manifest, which is
  `ManifestNotStored`. It hashes on the calling thread; use `spawn_blocking`.
- **Errors.** A manifest Steam no longer serves triggers one re-read of the
  product info, which moves every depot and the build id to the new build so
  the install is never mixed; if a manifest still cannot be fetched, the run
  fails with `ManifestChanged` carrying both ids, or with `ManifestNotServed`
  when the fresh read names the same id, which is also how a request code of 0
  ends. Rate limits from the CM or
  the CDN are `AuthError::RateLimited`; `DepotJob` retries a failed chunk fetch
  a bounded number of times and nothing here loops. `DiskFull` keeps the
  partial state; run the install again after freeing space.

An interrupted `update` leaves the `.acf` at the old manifest over files that
are partly new. The record says so; see "Follow-ups".

### The CLI and RPC

`crates/applejack-core/src/{steam,install}.rs` hold this; `applejack steam
login|logout|library`, `applejack install`, `steam.*`, and `app.install` are
thin over them. A `SteamHost` supplies the connection and the store: `LiveSteam`
for `applejack` and `serve`, and `NoSteam`, which reaches nothing, for a handler
built with `CoreHandler::new`, so a test that walks the method table cannot
touch an account or the Keychain.

`applejack steam login` asks for the account name, then the password through
`rpassword`, with echo off. `--qr` prints the payload instead. A code prompt
keeps polling, so typing the code and approving in the mobile app both finish
it. Codes are read with echo on: the reader thread is left blocked when the
login ends first, and a process that exits mid-read with echo off leaves the
terminal that way. `rpassword` clears echo, canonical mode, and `ISIG`, reads
control-C as a character, and raises `SIGINT` before its own restore, so the
login holds a handler from the password prompt on: `SIGINT`, `SIGTERM`, and
`SIGHUP` put the terminal back and exit with `128 + signal`.

`steam library` reads the cache, or Steam with `--refresh` or an empty cache.
Rows go to the `apps` table: `fetched_at` marks the cache, and `size_bytes`,
`default_branch`, and `latest_buildid` come with it. A refresh replaces the
cache in one transaction, and a title on disk keeps its `installdir` and stays
when it leaves the account. Over RPC a refresh is a job, because a large library
can outlast a request. `latest_buildid` is the default branch's, so a title on
another branch reads `installed` whatever it says.

`applejack install <appid> [--branch public] [--update] [--verify] [--library
<abs>]`, and `app.install`:

1. **Refusals**, quick and with no Steam, so each is an error response: another
   install of the title, an open session, a title on disk without `--update` or
   `--verify`, a stopped update without `--update`, a Windows client running.
2. **`login`**, then **`resolve`**: fresh product info for this one title with
   `fetch_owned_app`, an `InstallPlan`, and the launch choice below.
3. **`download`**, with byte counts. `update` re-reads the `.acf` into the row
   first. `verify` needs no login and hashes instead, as phase **`verify`**, and
   refuses with `VerifyFailed` when the `.acf` lists no depots.
4. **`register`**: the `.acf` into the `apps` row through `Core::record_title`,
   the one write `app register` uses too. Its `launch exe:` line names what a run
   would launch. A chosen exe that is missing or not a PE image is a `warning:`
   line naming `launch_exe_override`, and the install stands.

With no `--branch`, an update and a resumed install stay on `installed_branch`,
the branch recorded in the row; a new install takes the default. A title
`applejack install` did not put there has none and an update takes the default.

The job holds `locks/install-<appid>.lock` from the first step to the last and
the title's prefix lock with it. The prefix lock is what a run holds for the life
of a session, so an install and a run exclude each other across processes
whichever comes first. A run also refuses while a download record exists, which
is all that is left of a process that was killed; `app.run` checks again after
taking the lock. The first download writes the row's `library_dir` past the
installer's own refusals, just before the record, with no build id, so a run
finds the record of a title that has never finished and a refused install leaves
the row alone. `app cancel`, `app.cancel`, and `SIGINT` write
`locks/install-<appid>.cancel`, which the installer reads once a second from
whichever process runs it; a stop keeps the record, and the next install resumes
a first install while `--update` resumes an update, whose `.acf` still says whole.
The refusal names the right one. `--verify` reads no cancel file: it cannot be
stopped short of ending the process, though `app cancel` still answers that it
asked.

**Which exe.** Plan 05 says the app info's launch options filtered to
`oslist = windows`, overridable per game. `OwnedApp::launch_choice` applies, in
order: an entry with an executable, not a `server`, `editor`, `config`, or `vr`
type, whose `oslist` allows Windows, whose `betakey` is empty or names the
branch, and whose `ownsdlc` is owned; `osarch` matching the planned depots, unless
that leaves nothing; then `default` over untyped over `option1` and its like.
The entries that remain must agree on executable and arguments. If they do not,
or if the app has a `default` entry and these filters took it, so that what is
left was only ever offered beside it, the install refuses with
`LaunchExeAmbiguous` before a byte arrives and lists them;
`applejack config <appid> set launch_exe_override <file>` is the way out. A lone
entry stands only when no filter took a `default`. The rule errs towards refusing.
A title with an override, an exe already recorded, or a client launch
(`drm = steam` or `launch_mode = client`) needs no choice. Steamworks' uploading
documentation grounds only the OS and DLC filters. `type`, `osarch`, and
`betakey` are a reading of the field names, not a documented rule, and no
captured app info has been checked against this; `install` prints the exe it
chose so a wrong pick shows at once.

**`drm` stays `unknown`.** It cannot be inferred from Steam, and nothing here
marks a title `none`. The route to a client-free run is the user's word, after
trying it: `applejack run <appid> --probe-direct`, which launches once
whatever `drm` says, then `applejack config <appid> set drm none`, or
`config.set` with key `drm`. It lives on the title and not in `game_config`, so
a preset cannot carry it, and the refusal from `applejack run` names the route.

**`app register` stays**, for a title `applejack install` did not put there: a
library copied in, or one the Windows client installed, which `install` refuses
because a `.acf` is present. It writes through `Core::record_title`.

## Token handling rules

- The refresh token lives only in a Keychain generic-password item with
  service `applejack` and account `steam-refresh-token`, written through the
  Security framework (`SecItemAdd`, `SecItemCopyMatching`, `SecItemDelete`).
  The `security` command line tool is never invoked.
- The access token stays in memory (`auth::AccessToken`).
- `RefreshToken`, `AccessToken`, and `Password` redact themselves in `Debug`
  and `Display`; a value is readable only through `expose()`.
- No token is ever passed on a process command line. The crate constructs no
  `std::process::Command`: `crates/applejack-steam/clippy.toml` lists it and
  `tokio::process::Command` under `disallowed-types`, which the gate's Clippy
  run (`-D warnings`) enforces by resolved type, so an import alias cannot
  hide one; `tests/no_token_argv.rs` greps `src/` as a textual backstop, and
  also fails on a print of a line that names a token or depot key, in this
  crate, the core's RPC, Steam, and install modules, and the CLI's commands.
- `LoginMode` and `SteamLoginCompleteParams`, which carry a password and a code
  on the wire, write `Debug` by hand. So does every type above.
- The password buffer is not wiped. `Password` is a plain `String`, and the CLI
  drops it unwiped; over RPC the line buffer and the parsed copies keep it until
  they are reused. This is accepted: it stays in this process's memory, and the
  shell sends no password today.
- The Keychain item belongs to the macOS user, not the data directory, while the
  account name lives in each directory's `meta` table. `steam logout` with one
  `APPLEJACK_DATA_DIR` signs every other out, and a login in one leaves another
  naming the old account until its next login.
- `RefreshToken`, `AccessToken`, `Password`, and `Credentials` are not
  comparable (no `PartialEq`: a derived comparison is not constant time and
  nothing needs one), `License::Debug` omits the PICS `access_token`, and the
  `CMsgClientLogon` built in `SteamroomSession::start` carries the refresh
  token and is never logged or `Debug`-formatted.
- A depot key stays in memory for one run and is never written or printed. CDN
  auth tokens stay in `LiveChunks`, and no HTTP error leaves it with its URL.
  The record, stored manifests, and `.acf` hold no secret;
  `tests/depot.rs` scans what an install leaves on disk.
- Fixtures and docs contain no credential values. The fixture token in tests is
  the literal `fixture-refresh-token-not-a-real-value`.
- The `applejack` CLI and the copy embedded in the app are the same signed
  binary with identifier `com.applejack.core`, so both read the item without a
  prompt. An ad-hoc development build may prompt; that is acceptable.

## Locks

`lock::Lock` takes an advisory `flock` on `<data>/locks/<name>.lock`
(created with mode `0600`) and writes one JSON line `{"pid", "command",
"acquired_at"}` into it. The per-appid lock and `LockDir::library_exclusive`
take `LOCK_EX`; `AppLock::library` takes `LOCK_SH`. `command` is the fixed
subcommand label given to `LockDir::new` (`install`, `run`, ...), never argv
or user input. After `flock` succeeds the lock re-checks that the path still
names the locked inode and relocks otherwise, so unlinking a held lock file
does not admit a second holder. On release the holder line is truncated
before the unlock, unless it names another live process: shared holders
write into one file, so the line an exclusive contender sees is the last
shared holder's, and it is `None` once that holder released while others
remain.

Modes: `NonBlocking` fails at once with `Error::LockHeld { name, holder }`;
`Wait(duration)` polls and fails the same way on timeout; `Blocking` waits
without bound. `holder` is `None` when the file is empty (the new holder has
not written its line yet), when the line does not parse, or when the recorded
pid is dead (`kill(pid, 0)` reports `ESRCH`, so a line left by a crash is not
reported and a reused pid is only reported while that process exists).
`Lock::holder` reads the same record without locking and applies the same
filter.

`Wait(duration)` adds the duration to the current instant with a checked
add; a duration too large to represent (`Duration::MAX`) polls without a
deadline instead of panicking. A waiter polls the descriptor it already
holds open, so a `locks/` directory removed while it waits leaves it polling
the orphaned inode until the old holder lets go; `locks/` is recreated only
when `acquire` reopens the lock file after finding that the inode it locked
no longer sits at the path, so the waiter then relocks a fresh file. A
removal that races the `create_dir_all` or the open itself fails with
`LockIo` and is not retried. The holder in `LockHeld` is the last line
written into the file: after a crash, a reused pid names an unrelated
process, so the message says "last recorded holder" and nothing in applejack
kills it.

Lock order is per-appid first, then the library. Decision (U8e): the library
lock taken under an app lock is **shared**. "Shared library lock" in the plan
means `LOCK_SH`, so two `DepotInstaller` runs for different appids download at
the same time and serialize only against library-wide operations (rewriting
`libraryfolders.vdf`, moving the library), which take the library lock
exclusively through `LockDir::library_exclusive` and are never run while the
same process holds an app lock. The CLI and RPC never get that far: each opens
a Steam session, and the second is refused on `steam-session`. What the types
guarantee, within one process:

- `LockDir::app(&mut self, appid, mode)` yields an `AppLock` that borrows the
  `LockDir` mutably, so one `LockDir` holds at most one app lock, and the
  sequence app-1, library, app-2 on one `LockDir` does not compile (the
  `compile_fail` doc test on `LockDir::app` pins it).
- `AppLock::library(&mut self, mode)` yields a shared `LibraryLock`, which
  borrows the `AppLock` mutably and so cannot outlive it; one `AppLock` holds
  at most one `LibraryLock`, so a second `library()` call while the first is
  alive does not compile (the `compile_fail` doc test on `AppLock::library`
  pins it). One descriptor per holder keeps the holder record meaningful and
  keeps the shared lock from being mistaken for a reentrant one.
- `LockDir::library_exclusive(&mut self, mode)` yields an exclusive
  `LibraryLock` that borrows the `LockDir` mutably, so an app lock and the
  exclusive library lock cannot coexist on one `LockDir` (the `compile_fail`
  doc test on `LockDir::library_exclusive` pins it). It waits for every
  shared holder and keeps new ones out; `flock` conflicts between two open
  descriptions of one file even inside one process, so a process that held
  an app lock's shared library lock on another `LockDir` would wait on
  itself, which is why the rule is "never together with an app lock in one
  process". `Lock::acquire` and the library lock's name are crate-private;
  there is no other constructor.
- `LockDir` is not `Clone`.

What they do not guarantee: a process can construct two `LockDir`s and take
app-1 then library on one and app-2 on the other, and two processes can take
the locks in different orders. Across `LockDir`s and across processes the
order is a convention: a subcommand that needs several titles takes all of
its app locks first, in ascending appid order (one `LockDir` per app lock),
then the library lock from the `AppLock` it took last, and never an app lock
while holding the library lock; `Wait` rather than `Blocking` wherever a hung
holder must not wedge the caller. A typed multi-app acquire (`LockDir::apps`
over a sorted, deduplicated appid list) would be added if one process ever
needs more than one title; the installer holds one.

Every applejack process sends the same `LOGIN_ID` in `CMsgClientLogon`, so two
concurrent logged-in applejack connections on one account displace each
other. `core::steam::silent_logon` holds `LockDir::steam_session` for as long
as a session exists, non-blocking, so a second one fails at once naming the
first. Two installs of different titles therefore do not overlap, whatever the
library lock would allow: the second waits for the user, not for the first.

`LockDir::install_job` comes before the prefix lock, the prefix lock before
`steam-session`, and `steam-session` before the app lock.

These locks serialize applejack processes only. They say nothing about the
Windows Steam client.

## Ownership rule shared with work package 5

A title is written by applejack's depot code only while no Steam client is
running in any prefix that maps the shared library, where "running" means
`wineserver_alive(prefix)` from work package 2, not the advisory lock. That
function lives in `applejack-engine`, which this crate does not depend on, so
`DepotInstaller::new` takes it as a `client_running` predicate. It is read
under the locks on entry, again before the first write and before each depot,
once a second while downloading, and before stale files are removed and the
`.acf` written.
While it is true, the run stops with `Error::ClientRunning`. `DepotJob` runs
each chunk under `tokio::spawn` with no way to cancel them, so a dropped
download would leave them writing past the locks and, through a shared staging
inode, into a later run's finished file. The installer never drops it: a stop
ends every fetch at once, the download joins its chunk tasks and returns, the
record is saved, and only then do the locks go. A caller that drops the
install future gets the same stop, and the task that outlives it keeps the
locks until the last writer has finished. Until `DepotJob` returns, a file
whose chunks are all on disk needs no fetch and so still lands: a stop takes
effect at the next chunk that has to be fetched, and everything before it
happens under the locks. While a client runs,
applejack treats the library as read-only and re-syncs its `apps` rows from
the `.acf` files after `wineserver_alive` turns false. An
`appmanifest` whose `StateFlags` is not `4` (`AppManifest::is_fully_installed`
is false) belongs to the client; `install --update` refuses and names
`applejack runtime steam` as the owner.

## `.acf` and `.vdf`

`vdf::Document` reads Valve's text KeyValues: quoted and bare tokens, nested
braces up to `vdf::MAX_DEPTH` (64; deeper input is `Error::VdfParse`, not a
stack overflow), `//` comments, a leading UTF-8 BOM, the escapes `\" \\ \n
\t \r` (any other escape is `VdfParse`, since the writer would change its
bytes), and duplicate keys preserved in order. A `[$CONDITION]` suffix is
skipped, never evaluated, only where it follows a value on the same line or
a key whose value is a `{` block; a `[` anywhere else is part of a bare token
(`a[b]` is one token, `v [b]` is a value and a conditional). Top-level
`#base` and `#include` directives are rejected with `VdfParse`. The writer
emits Valve's layout with LF line ends. A client-written file (LF, no BOM,
control characters inside quoted strings escaped, which is what Valve's
writer produces) round-trips byte for byte; input the client does not write
is normalised: CRLF between tokens becomes LF, a leading BOM is dropped, and
a raw tab, CR, or newline inside a quoted string is written back as its
escape. `AppManifest::parse` rejects a `Universe` other than `1` with
`AcfField`, so a manifest from another universe is never rewritten as
public.
`acf::AppManifest` reads and `acf::AcfWriter`
writes `appmanifest_<appid>.acf` with `appid`, `Universe` (always `1`, the
public universe; some client versions treat its absence as a foreign
manifest), `name`, `StateFlags`, `installdir`, `LastUpdated`, `SizeOnDisk`,
`buildid`, and `InstalledDepots` (`manifest`, `size` per depot). The field
names and order follow the public format as documented by SteamKit and
SteamDB. `StateFlags` is the `EAppState` bitfield (`acf::app_state` exposes
the bits: 1 Uninstalled, 2 UpdateRequired, 4 FullyInstalled, 64 AppRunning,
256 UpdateRunning, 1048576 Downloading, ...); `is_fully_installed` compares
the whole value with `4`, so `6`, `68`, and `1030` all refuse.
`AcfWriter::write` emits the modelled keys only; `AcfWriter::merge`, used for
`update`, keeps every key of the existing file. This field list is
provisional until work package 5 captures a real file.
`tests/fixtures/appmanifest_000000.acf` is hand-written and redacted (appid
0); work package 5 replaces it with a captured client file and extends the
writer with whatever keys the client needs.

## Follow-ups

- **Verify a client-installed depot.** `verify_install` needs a stored manifest;
  a session-aware variant could fetch it.
- **Power loss.** Resume trusts a recorded file's size. `DepotJob` does not
  fsync before its rename, so after a power cut `verify_install` is the check.
- **Repair.** `VerifyReport` only reports; `install` refuses an installed title.
- **PICS over HTTP.** A reply can carry `http_host` and omit a large buffer.
  Such an app lands in `Library::skipped`. Whether Steam still does this for
  this request is unknown until a large account refreshes its library.
- **Family sharing** is out of scope, and `Library` does not yet filter
  licenses by `owner_id`, since the value on an account's own licenses has not
  been observed. Expired licenses are not filtered either; the `ELicenseFlags`
  bits are not in `steamroom`, and such an app fails at install with a depot
  access error.

Upstream gaps in `steamroom` and `steamroom-client` 0.3.0, for the operator
to file; the local copies in `live.rs` stay until they land:

1. `TokenLogin::login` (`steamroom-client-0.3.0/src/login/terminal.rs:55-70`)
   never sends `config.login_id`, so applejack composes `CMsgClientLogon`
   itself to carry the fixed `LOGIN_ID`.
2. The same builder omits `supports_rate_limit_response`, so a throttled
   logon comes back as a generic failure rather than the typed EResult.
3. `SteamClient<Ready>::login` (`steamroom-0.3.0/src/client/mod.rs:286-350`)
   discards the sibling messages of the `MULTI` that carries the logon
   response; applejack avoids `login` entirely (see "Session").
4. `GuardChallenge::submit_code` (`steamroom-client-0.3.0/src/login/credentials.rs:167-187`)
   treats `DuplicateRequest` as fatal, and `GuardChallenge` exposes no poll.
   `live.rs` makes the same `SteamClient<Ready>` calls itself instead, and
   `poll_auth_session` drops `new_client_id` and `new_challenge_url`, so it
   calls `PollAuthSessionStatus` itself too.
5. Each `SteamClient<LoggedIn>` call runs its own receive loop, so they cannot
   share a connection with anything else that reads. A receive buffer in
   `ClientInner`, or a dispatcher, would let applejack drop the wire copies in
   `live/session.rs`.
6. `get_cdn_servers` is only on `LoggedIn`, and `CdnServer` is
   `#[non_exhaustive]` with no constructor, so `live/cdn.rs` builds one through
   its `serde` form.
7. `CdnClient` puts the auth token in the URL with no timeout, and
   `CdnChunkFetcher` logs and returns `reqwest` errors that contain it.
8. `pics_get_package_info` hard-codes `access_token: 0`
   (`steamroom-0.3.0/src/client/mod.rs:731`) and every PICS call ignores
   `response_pending`.
9. `DepotJob` spawns its chunk tasks with no way to cancel them and no check
   between files, so applejack ends them by failing every fetch, and a file
   that needs no fetch still lands after a stop.
10. `get_cdn_servers` builds URLs on `host` and takes HTTPS for `optional`,
    where `SteamKit` uses `vhost` and HTTPS only for `mandatory`.
11. `parse_incoming` drops the job ids of a non-protobuf header, so a top-level
    `DestJobFailed` in that form cannot be routed and its call waits out the
    60 s bound. One inside a `MULTI` is no different: `PacketHeader::parse`
    returns `Simple` for every non-protobuf packet, so the `Extended` arm of
    `parse_packet` never runs and the ids are lost there too.

## Legal note

The Steam Subscriber Agreement (revision 2026-09-10) §2.G prohibits emulating
Valve's communication protocols without consent; SteamKit2 and DepotDownloader
have been tolerated for years. The plan records this as a legal risk the user
accepts. This layer acts on the user's own account for the user's own
purchases only and does not bypass DRM.

## Live result

On 2026-10-31 the depot spike, since deleted, passed against a real account at
e615214: device-code login, CM logon, license and product info, then Spacewar
(480) from two depots, one of them shared from app 228980. It wrote 12 files and
85,850,313 bytes and verified them with no mismatches.

Both spikes are gone: `steam login`, `steam library`, and `install` are the same
flows. The CLI itself has not run against live Steam yet.

| Date | Gate G2 | steamroom | steamroom-client | Notes |
|------|---------|-----------|------------------|-------|
| 2026-10-30 | green | 0.3.0 | 0.3.0 | macOS 26.6.2, `--credentials` with a Steam Guard device code |

Green: the Rust crates stay primary. Red: DepotDownloader becomes primary for
the first release and the Rust path is deferred in the handoff.

## API grounding

Registry sources read on 2026-10-28 under
`~/.cargo/registry/src/index.crates.io-*/`. Plan names that differ from the
real ones are listed in the unit report's DEVIATIONS.

| Plan name | Real name | File:line |
|-----------|-----------|-----------|
| credential auth begin | `SteamClient<Ready>::begin_auth_session_via_credentials` | `steamroom-0.3.0/src/client/mod.rs:422` |
| QR auth begin | `SteamClient<Ready>::begin_auth_session_via_qr` | `steamroom-0.3.0/src/client/mod.rs:446` |
| poll | `SteamClient<Ready>::call_service_method_non_authed` with `Authentication.PollAuthSessionStatus#1`, as `poll_auth_session` does | `steamroom-0.3.0/src/client/mod.rs:370,470` |
| Steam Guard submit | `SteamClient<Ready>::submit_steam_guard_code` | `steamroom-0.3.0/src/client/mod.rs:499` |
| tokens after approval | `CAuthenticationPollAuthSessionStatusResponse { access_token, refresh_token, account_name, new_client_id, new_challenge_url }` | generated from `steamroom-0.3.0/proto` |
| refresh-token logon | `SteamClient<Ready>::send_msg` with `CMsgClientLogon { account_name, access_token, obfuscated_private_ip, .. }`, the reply routed by the session's receive loop (`login` drops `MULTI` siblings) | `steamroom-0.3.0/src/client/mod.rs:362`; proto `steammessages_clientserver_login.proto` (`account_name`=50, `access_token`=108, `obfuscated_private_ip`=11) |
| (reference) | `TokenLogin::login` builds the same message without a logon id and without `supports_rate_limit_response`, both of which applejack adds | `steamroom-client-0.3.0/src/login/terminal.rs:55` |
| logon response | `CMsgClientLogonResponse { eresult, heartbeat_seconds, .. }` from the `IncomingMsg` that `login` returns; `SteamClient<LoggedIn>::send_heartbeat` | proto `steammessages_clientserver_login.proto:97`; `steamroom-0.3.0/src/client/mod.rs:289,537` |
| license-list result | `CMsgClientLicenseList.eresult` (`optional int32`, default 2) checked through `steamroom::enums::eresult` | `steamroom-0.3.0/proto/steam/steammessages_clientserver.proto:188`; `src/enums/mod.rs:79` |
| PICS access tokens | `SteamClient<LoggedIn>::pics_get_access_tokens` | `steamroom-0.3.0/src/client/mod.rs:578` |
| PICS product info | `SteamClient<LoggedIn>::pics_get_product_info` | `steamroom-0.3.0/src/client/mod.rs:625` |
| PICS package access tokens | `SteamClient<LoggedIn>::pics_get_package_access_tokens` | `steamroom-0.3.0/src/client/mod.rs:681` |
| PICS package info | `SteamClient<LoggedIn>::pics_get_package_info` | `steamroom-0.3.0/src/client/mod.rs:731` |
| `recv_msg`, `send_msg` (used) | `SteamClient<Ready>::recv_msg`, `send_msg` (also on `LoggedIn`) | `steamroom-0.3.0/src/client/mod.rs:366,362` |
| depot download | `steamroom_client::download::{DepotJob, DepotJobBuilder, ChunkFetcher, OldChunkLoc, RetryConfig, DownloadError, DownloadReport}` | `steamroom-client-0.3.0/src/download.rs` |
| manifest decode | `steamroom_client::manifest::parse_cdn_manifest`, `DepotManifest::decrypt_filenames` | `steamroom-client-0.3.0/src/manifest.rs:65`, `steamroom-0.3.0/src/depot/manifest.rs` |
| CDN | `CdnClient::{download_chunk, download_manifest}`, `CdnServerPool::{pick, report_success, report_failure}` | `steamroom-0.3.0/src/cdn/mod.rs`, `src/cdn/pool.rs` |
| KeyValues | `KeyValue::from_text`, `key_value::parse_binary_kv` | `steamroom-0.3.0/src/types/key_value.rs:78,111` |
| `EMsg::CLIENT_LICENSE_LIST` | `EMsg::CLIENT_LICENSE_LIST = EMsg(780)` | `steamroom-0.3.0/src/messages/mod.rs:27` |
| `CMsgClientLicenseList` | `steamroom::generated::CMsgClientLicenseList` (prost, `build.rs`) | `steamroom-0.3.0/proto/steam/steammessages_clientserver.proto:167`; `src/generated/mod.rs:1` |
| MULTI unpacking | `steamroom::client::multi::unpack_multi`, `messages::header::PacketHeader::parse` | `steamroom-0.3.0/src/client/multi.rs:10`, `src/messages/header.rs:53` |
| CM connect | `CmServer::fetch`/`defaults`, `TcpTransport::connect`, `SteamClient::connect`, `encrypt`, `prepare` | `steamroom-0.3.0/src/connection/mod.rs:42,35`, `src/transport/tcp.rs:21`, `src/client/mod.rs:97,143,276` |
| fixed logon id | `CMsgClientLogon.obfuscated_private_ip: Option<CMsgIpAddress>`, `c_msg_ip_address::Ip::V4(u32)` | generated from `steammessages_base.proto:64` |
| EResult mapping | `steamroom::enums::EResultError::{RateLimitExceeded, LoginDeniedThrottle, TwoFactorRequired, TwoFactorCodeMismatch, InvalidPassword, AccessDenied, Expired, Revoked, Banned}` | `steamroom-0.3.0/src/enums/mod.rs:33-72` |
| `fd-lock` / `rustix` flock | `rustix::fs::flock(fd, FlockOperation::{LockShared, LockExclusive, NonBlockingLockShared, NonBlockingLockExclusive, Unlock})` | `rustix-1.1.6/src/fs/fd.rs:322`, `src/backend/libc/fs/types.rs:946-973` (what `fd-lock-4.0.4/src/sys/unix/rw_lock.rs:20-32` calls) |
| `set_generic_password` | `security_framework::passwords::set_generic_password(service, account, &[u8])` | `security-framework-3.7.0/src/passwords.rs:26` |
| `find_generic_password` | `security_framework::passwords::generic_password(PasswordOptions::new_generic_password(..))` (SecItem API) | `security-framework-3.7.0/src/passwords.rs:53`, `src/passwords_options.rs:63` |
| delete | `security_framework::passwords::delete_generic_password(service, account)` | `security-framework-3.7.0/src/passwords.rs:63` |
| not-found code | `errSecItemNotFound = -25300` | `security-framework-sys-2.17.0/src/base.rs:61` |
| password prompt | `rpassword::prompt_password` | `rpassword-7.5.4/src/lib.rs:270` |
