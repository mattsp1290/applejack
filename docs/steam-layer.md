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

Approving the mobile prompt and then typing a code fails with EResult
`DuplicateRequest`, on the first attempt of this run. SteamKit treats that
result as success because the next poll returns tokens; `steamroom-client`
treats it as fatal. applejack surfaces it as
`AuthError::DuplicateGuardApproval`, which ends the login and tells the user
to log in again with one factor. `GuardChallenge` keeps its client and poll
loop private, so continuing would need a `[patch.crates-io]` fork of
`steamroom-client`; a fork is a permanent rebase cost for a failure the user
clears by retrying, so applejack does not carry one.

## Flows

All flows go through `auth::Authenticator`, which drives a `SteamConnection`
and a `RefreshTokenStore`. `SteamConnection` covers the auth flow only;
`logon` returns a `SteamSession`, described under "Session". Unit tests use
scripted fakes; `live::SteamroomConnection` and `live::SteamroomSession` are
the only implementations that open a socket.

- **QR login.** `begin(AuthMode::Qr)` calls `BeginAuthSessionViaQR` and returns
  `LoginStep::QrReady { payload }`; the shell renders the payload as a QR code.
  `finish()` polls `PollAuthSessionStatus` until the Steam mobile app approves,
  stores the refresh token, and returns `LoginStep::Done { account_name }`.
- **Credentials login.** `begin(AuthMode::Credentials, Some(credentials))`
  fetches the account's RSA key, sends the encrypted password to
  `BeginAuthSessionViaCredentials`, and returns either `Done` (no second
  factor), `GuardRequired { factor: EmailCode | DeviceCode }` (call
  `submit_guard_code`, which may answer `InvalidGuardCode` and keep the session
  open), or `GuardRequired { factor: DeviceConfirmation }` (call `finish()`
  and wait for the mobile approval). Any error other than `InvalidGuardCode`
  from `submit_guard_code` or `finish` (a rate limit, a transport failure, a
  missing field, a store that rejects the token) ends the login: the
  connection has consumed its auth session, the driver returns to idle, and
  the next call must be `begin` again; `finish` or `submit_guard_code` before
  that is `OutOfOrder`, never `GuardRequired`, so a shell that re-prompts on
  `GuardRequired` cannot loop. The same holds for a `GuardRequired` that the
  connection itself reports from `finish`: it ends the login, since the
  driver's own `GuardRequired` is only ever returned before the flow is
  consumed. A call in the wrong state is `OutOfOrder` and leaves the
  connection's state unchanged; a live connection never answers
  `InvalidGuardCode` once its challenge is gone (a mismatch reported by the
  token poll rather than the submit is `OutOfOrder`), so "try another code"
  is only ever said while another code can be accepted.
- **Refresh.** `refresh(account_name)` reads the stored refresh token, sends
  `CMsgClientLogon` with it on a fresh connection, and returns the session.
  The account name is not a secret; the token logon requires it alongside
  the token, so the host persists it next to its other metadata (the `meta`
  table in work package 6). `AuthError::NoStoredToken` means the user must
  log in; `AuthError::Refused` means the token was rejected and the CLI
  exits 3.
- **Logout.** `logout()` deletes the Keychain item. Clearing cached library
  rows is the host's job.

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
  `without_url`; and because a token is per depot and host, requested the
  when a host answers 403, whether it never had one or its token expired, and
  at most once in 30 s per depot and host.

## Library

`library::Library::fetch` walks licenses to packages (`package_apps`, a binary
`KeyValues` buffer behind a 4-byte prefix) to appids to PICS product info
(`app_info`, text `KeyValues`, access tokens included). `Library` returns plain
values; the caller persists them to the `apps` table with `fetched_at`.

`OwnedApp { appid, name, installdir, size, default_branch, depots, drm,
platforms, builds, change_number }` describes one app. `drm` is always
`unknown`: it cannot be inferred from Steam, and the config database overrides
it. `Library::windows_apps` is the installable list.
`Library::windows_app(appid)` answers `AppNotOwned`, or `NoWindowsDepot` naming
the platforms the app lists.

`OwnedApp::windows_depots(branch, filter)` picks what to fetch: depots whose
`oslist` includes `windows` or is empty, on an app whose own `oslist` allows
Windows; `osarch` 64 and neutral, or 32 when the app has no 64-bit depot; the
filter's language (`english` by default) or none; no `lowviolence` depots; a
`dlcappid` depot only when that DLC is owned. A depot with no manifest on the
branch is skipped, which also leaves out `depotfromapp` redistributables.

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
  target that would leave the install directory (`Error::UnsafePath`).
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
  unless another installed depot still owns them. Names compare without case,
  since APFS is case-insensitive. Removal is skipped altogether when an
  untouched depot's manifest is not stored, because nothing can then be proved
  stale. A depot already current is left alone, unless a stopped run's record
  names it: such a depot is rechecked in full whatever the `.acf` says, since
  the `.acf` is what the stopped run did not move. A recorded depot the plan
  no longer selects goes back to its `.acf` manifest, or is refused with
  `Error::UnfinishedDepot` when the `.acf` does not list it, and the record
  stays. Files only an abandoned target had are removed as stale. The rewrite
  keeps every `.acf` key the writer does not model, and keeps depots the plan
  did not select. The caller re-syncs its `apps` row from the `.acf` first and
  afterwards.
- **Overlapping depots.** A path is on disk once, and the depot with the
  highest id wins; `path_owners` is the one place that says so, for what to
  write and for verify. Numeric order is a choice: `KeyValue` keeps children in
  a map, so the buffer's own order is lost. A delta queues a path only for its
  owner, so it never overwrites a later depot's overlay, and a depot that drops
  an overlay brings the lower depot's file back, even when that depot did not
  change.
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
  fails with `ManifestChanged` carrying both ids. Rate limits from the CM or
  the CDN are `AuthError::RateLimited`; `DepotJob` retries a failed chunk fetch
  a bounded number of times and nothing here loops. `DiskFull` keeps the
  partial state; run the install again after freeing space.

An interrupted `update` leaves the `.acf` at the old manifest over files that
are partly new. The record says so; see "Follow-ups".

### What the CLI unit wires

- `applejack steam library [--json] [--refresh]`: `Library::fetch`,
  `windows_apps`, and the `apps` table.
- `applejack install <appid> [--branch public]`: `InstallPlan::new` from fresh
  product info, `DepotInstaller::install`. `--update` is `update`, after
  syncing the row from the `.acf`. `--verify` is `verify_install`.
- `client_running`: `wineserver_alive` over every prefix that maps the library.
- The `steam-session` lock for as long as a session lives.
- `InstallProgress`, through `with_progress`.
- Exit codes and messages for `Error`'s variants.

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
  hide one; `tests/no_token_argv.rs` greps `src/` and `examples/` as a
  textual backstop, and also fails on a print of a line that names a token or
  depot key.
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
means `LOCK_SH`, so two `applejack install` processes for different appids
download at the same time and serialize only against library-wide
operations (rewriting `libraryfolders.vdf`, moving the library), which take
the library lock exclusively through `LockDir::library_exclusive` and are
never run while the same process holds an app lock. What the types
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
other. The CLI/RPC unit that owns the connection lifetime must hold a
`steam-session` lock (a `LockDir` name) for as long as a `SteamroomSession`
exists; this crate only documents that rule.

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

- **Interrupted update.** The `.acf` stays at the old manifest while files
  change. Launch code should check `LibraryPaths::record` and refuse a title
  with a record present until the install is resumed.
- **Verify a client-installed depot.** `verify_install` needs a stored manifest;
  a session-aware variant could fetch it.
- **Power loss.** Resume trusts a recorded file's size. `DepotJob` does not
  fsync before its rename, so after a power cut `verify_install` is the check.
- **Repair.** `VerifyReport` only reports; `install` refuses an installed title.
- **`depotfromapp` depots** are skipped. If a title turns out to need one, read
  the manifest from the other app's product info.
- **PICS over HTTP.** A reply can carry `http_host` and omit a large buffer.
  Such an app lands in `Library::skipped`. Whether Steam still does this for
  this request is unknown until a large account runs the spike.
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
   treats `DuplicateRequest` as fatal, and `GuardChallenge` exposes no poll,
   so a caller cannot continue after the user approved in the app.
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

## Legal note

The Steam Subscriber Agreement (revision 2026-09-10) §2.G prohibits emulating
Valve's communication protocols without consent; SteamKit2 and DepotDownloader
have been tolerated for years. The plan records this as a legal risk the user
accepts. This layer acts on the user's own account for the user's own
purchases only and does not bypass DRM.

## Running the depot spike

`examples/depot_spike.rs` is the live check of this code. Without
`APPLEJACK_LIVE_STEAM=1` it prints usage, exits 0, and contacts nothing.

```sh
cargo run -p applejack-steam --example depot_spike                 # prints usage, exits 0
APPLEJACK_LIVE_STEAM=1 cargo run -p applejack-steam --example depot_spike
APPLEJACK_LIVE_STEAM=1 cargo run -p applejack-steam --example depot_spike -- \
    --install 480 --dir /tmp/applejack-spike
```

Spacewar (480) is a small free title; add it to the account first, or use any
small Windows title the account owns. `--dir` is the library root, and it also
holds `locks/`. The spike reads the account name from stdin and the password
from the terminal without echo. When Steam asks for a code, enter the code and
do not also approve in the mobile app; doing both fails with
`DuplicateRequest`. It holds the refresh token in memory only and never prints
it. It passes a `client_running` that is always false, so quit any client that
maps the directory first.

Green is `logon ok`, a plausible `owned apps with a Windows depot` count, and
with `--install`, `verify: N files, 0 mismatched, 0 missing` and `depot spike:
green`. Interrupt an install and run it again to see `resumed: true`.

## Running the G2 spike

```sh
cargo run -p applejack-steam --example auth_spike                 # prints usage, exits 0
APPLEJACK_LIVE_STEAM=1 cargo run -p applejack-steam --example auth_spike
APPLEJACK_LIVE_STEAM=1 cargo run -p applejack-steam --example auth_spike -- --credentials
```

Use `--credentials` as the primary G2 mode. QR mode prints the challenge URL
to render with any QR tool and scan with the Steam mobile app, but
`steamroom` 0.3.0 ignores the `new_client_id` and `new_challenge_url` fields
of `CAuthentication_PollAuthSessionStatus_Response`
(`steamroom-0.3.0/src/client/mod.rs:470-496`), so when the QR challenge
rotates the printed URL goes stale and the poll keeps running with the old
id; scan within about 30 s or fall back to `--credentials`. Credentials mode
reads the account name from stdin and the password from the terminal without
echo (`rpassword`). The spike stores the refresh token in an `InMemoryStore`,
logs on again with it, prints the heartbeat interval from the logon response,
decodes the license list, prints the license count and crate versions, and
never prints the token.

### Reading the result

- `login ok`, `refresh logon ok`, `licenses on account: N` with `N >= 1`, and
  `gate G2: green`: green. A logged-on account always holds package 0, so the
  spike prints a warning instead of the green line when the list is empty;
  that is a decode problem to fix, not a red result.
- A failure before `refresh logon ok` (auth, Steam Guard, rate limit,
  `CMsgClientLogonResponse` eresult): the real red case. Rate limits and
  Steam Guard surface as typed errors; retry after the floor or complete the
  factor rather than counting them as red.
- `refresh logon ok` followed by `Steam EResult ...`, `rate limited by
  Steam`, or `Steam refused the login` from the license list: the CM
  accepted the logon but answered `CMsgClientLicenseList` with a non-OK
  `eresult`. Red for the decode path; record the EResult in the table.
- `refresh logon ok` followed by `timed out waiting for CLIENT_LICENSE_LIST`:
  **not red**. This is the known MULTI-drain behaviour of
  `SteamClient<Ready>::login` (`steamroom-0.3.0/src/client/mod.rs:286-350`):
  it returns on the first `CLIENT_LOG_ON_RESPONSE`, discards every other
  message in the same `MULTI`, and the `Receiver` from `SteamClient::connect`
  is dead on arrival (`mod.rs:97-118`). The CM normally sends the logon
  response as its own packet and the license list in a later `MULTI`, which
  `license_list` unpacks, but that is server behaviour, not a contract.
  Fallback order:
  1. For the spike: a hand-rolled logon on `SteamClient<Ready>` with
     `send_msg(ClientMsg::with_body(EMsg::CLIENT_LOGON, ..))` and a
     `recv_msg` loop that unpacks `MULTI` and captures both
     `CLIENT_LOG_ON_RESPONSE` (check `CMsgClientLogonResponse.eresult`, read
     `steamid`/`client_sessionid` from the header) and `CLIENT_LICENSE_LIST`.
     That proves the decode and keeps the Rust path green. Its limit: it never
     yields `SteamClient<LoggedIn>`, so `pics_get_*` (all on `LoggedIn`,
     `mod.rs:578-731`) are unavailable on that connection.
  2. The durable fix is upstream: a `login` variant that returns the drained
     sibling messages (or a receive buffer in `ClientInner`), applied through
     `[patch.crates-io]` on a fork. A second connection per run is the only
     alternative and doubles Steam Guard and rate-limit exposure.
  3. `steam-vent` becomes relevant only if the hand-rolled loop also never
     sees `CLIENT_LICENSE_LIST`.

Record the outcome here:

| Date | Outcome | steamroom | steamroom-client | Notes |
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
| (wrapper used) | `LoginBuilder::with_credentials` / `CredentialsLogin::begin` | `steamroom-client-0.3.0/src/login/mod.rs:292`, `src/login/credentials.rs:58` |
| QR auth begin | `SteamClient<Ready>::begin_auth_session_via_qr` | `steamroom-0.3.0/src/client/mod.rs:446` |
| (wrapper used) | `LoginBuilder::with_qr` / `QrLogin::begin` / `QrLoginFlow::challenge_url` / `wait_for_scan` | `steamroom-client-0.3.0/src/login/mod.rs:305`, `src/login/qr.rs:37,73,85` |
| poll | `SteamClient<Ready>::poll_auth_session` | `steamroom-0.3.0/src/client/mod.rs:470` |
| (wrapper used) | `poll_until_tokens` (via `wait_for_scan`, `wait_for_confirmation`, `submit_code`) | `steamroom-client-0.3.0/src/login/credentials.rs:226` |
| Steam Guard submit | `SteamClient<Ready>::submit_steam_guard_code` | `steamroom-0.3.0/src/client/mod.rs:499` |
| (wrapper used) | `GuardChallenge::submit_code`, `MobileChallenge::wait_for_confirmation` | `steamroom-client-0.3.0/src/login/credentials.rs:167,207` |
| tokens after approval | `ApprovedAuth::tokens` → `steamroom::auth::AuthTokens { access_token, refresh_token, account_name }` | `steamroom-client-0.3.0/src/login/terminal.rs:83`, `steamroom-0.3.0/src/auth/mod.rs:23` (line 22 is its `#[non_exhaustive]`) |
| refresh-token logon | `SteamClient<Ready>::login(ClientMsg)` with `CMsgClientLogon { account_name, access_token, obfuscated_private_ip, .. }` | `steamroom-0.3.0/src/client/mod.rs:286`; proto `steammessages_clientserver_login.proto` (`account_name`=50, `access_token`=108, `obfuscated_private_ip`=11) |
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
