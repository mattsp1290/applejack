# Steam layer

`crates/applejack-steam` owns authentication, refresh-token storage, the
owned-license list, `.acf` and `.vdf` handling, and the advisory locks that
serialize applejack processes. It depends on `applejack-manifest` and external
crates only, never on `applejack-config`; it accepts and returns plain structs.

Gate G2 status: **not yet run**. The spike builds (`examples/auth_spike.rs`);
the operator runs it with their own account and records the outcome below.

## Flows

All flows go through `auth::Authenticator`, which drives a `SteamConnection`
and a `RefreshTokenStore`. Unit tests use a scripted fake connection;
`live::SteamroomConnection` is the only implementation that opens a socket.

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
  `GuardRequired` cannot loop.
- **Refresh.** `refresh(account_name)` reads the stored refresh token and sends
  `CMsgClientLogon` with it on a fresh connection. The account name is not a
  secret; `steamroom-client`'s token logon requires it alongside the token, so
  the host persists it next to its other metadata (the `meta` table in work
  package 6). `AuthError::NoStoredToken` means the user must log in;
  `AuthError::Refused` means the token was rejected and the CLI exits 3.
- **Logout.** `logout()` deletes the Keychain item. Clearing cached library
  rows is the host's job.
- **License list.** After `refresh`, `SteamConnection::license_list` reads
  `EMsg::CLIENT_LICENSE_LIST` from the same connection (also inside `MULTI`
  batches) and decodes `CMsgClientLicenseList`. It times out after 30 s.
  Call it exactly once, directly after `logon` and before any PICS or
  heartbeat call: the live implementation drops every other message it
  receives while waiting, and Steam re-sends the list unprompted whenever
  the account's licenses change, so once PICS calls and heartbeats share
  the session a stray `license_list` can eat a PICS reply and a PICS call
  can eat the list. The unit that shares the session must own one
  receive-and-dispatch loop (see the design note under "Follow-ups").

Rate limits and Steam Guard surface as typed errors (`AuthError::RateLimited
{ retry_after }`, `AuthError::GuardRequired { factor }`); nothing loops on
them. The CM does not report a retry-after, so the value is a fixed 60 s floor.

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
  textual backstop.
- `RefreshToken` is not comparable (no `PartialEq`), `License::Debug` omits
  the PICS `access_token`, and the `CMsgClientLogon` built in `live::logon`
  carries the refresh token and is never logged or `Debug`-formatted.
- Fixtures and docs contain no credential values. The fixture token in tests is
  the literal `fixture-refresh-token-not-a-real-value`.
- The `applejack` CLI and the copy embedded in the app are the same signed
  binary with identifier `com.applejack.core`, so both read the item without a
  prompt. An ad-hoc development build may prompt; that is acceptable.

## Locks

`lock::Lock` takes an exclusive `flock` on `<data>/locks/<name>.lock`
(created with mode `0600`) and writes one JSON line `{"pid", "command",
"acquired_at"}` into it. `command` is the fixed subcommand label given to
`LockDir::new` (`install`, `run`, ...), never argv or user input. After
`flock` succeeds the lock re-checks that the path still names the locked
inode and relocks otherwise, so unlinking a held lock file does not admit a
second holder. On release the holder line is truncated before the unlock.

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
deadline instead of panicking. `locks/` is recreated on every poll, so a
directory removed while a caller waits does not end the wait. The holder in
`LockHeld` is the last line written into the file: after a crash, a reused
pid names an unrelated process, so the message says "last recorded holder"
and nothing in applejack kills it.

Lock order is per-appid first, then the library. What the types guarantee,
within one process:

- `LockDir::app(&mut self, appid, mode)` yields an `AppLock` that borrows the
  `LockDir` mutably, so one `LockDir` holds at most one app lock, and the
  sequence app-1, library, app-2 on one `LockDir` does not compile (the
  `compile_fail` doc test on `LockDir::app` pins it).
- Only `AppLock::library(&mut self, mode)` yields a `LibraryLock`, which
  borrows the `AppLock` mutably and so cannot outlive it; one `AppLock` holds
  at most one `LibraryLock`, so a second `library()` call while the first is
  alive does not compile (the `compile_fail` doc test on `AppLock::library`
  pins it). `flock` conflicts between two open descriptions of one file even
  inside one process, so without that rule the second call would wait on
  itself. `Lock::acquire` and the library lock's name are crate-private; there
  is no other constructor.
- `LockDir` is not `Clone`.

What they do not guarantee: a process can construct two `LockDir`s and take
app-1 then library on one and app-2 on the other, and two processes can take
the locks in different orders. Across `LockDir`s and across processes the
order is a convention: a subcommand that needs several titles takes all of
its app locks first, in ascending appid order (one `LockDir` per app lock),
then the library lock from the `AppLock` it took last, and never an app lock
while holding the library lock; `Wait` rather than `Blocking` wherever a hung
holder must not wedge the caller. A typed multi-app acquire (`LockDir::apps`
over a sorted, deduplicated appid list) is a follow-up for the depot unit if
it needs more than one title per process.

Every applejack process sends the same `LOGIN_ID` in `CMsgClientLogon`, so two
concurrent logged-in applejack connections on one account displace each
other. The CLI/RPC unit that owns the connection lifetime must hold a
`steam-session` lock (a `LockDir` name) for as long as a `SteamroomConnection`
is in the logged-in stage; this crate only documents that rule.

These locks serialize applejack processes only. They say nothing about the
Windows Steam client.

## Ownership rule shared with work package 5

A title is written by applejack's depot code only while no Steam client is
running in any prefix that maps the shared library, where "running" means
`wineserver_alive(prefix)` from work package 2, not the advisory lock. While a
client runs, applejack treats the library as read-only and re-syncs its `apps`
rows from the `.acf` files after `wineserver_alive` turns false. An
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
the whole value with `4`, so `6`, `68`, and `1030` all refuse. Keys the
writer does not model are dropped on rewrite; the depot unit, which rewrites
client-written files for `install --update`, retains them.
`tests/fixtures/appmanifest_000000.acf` is hand-written and redacted (appid
0); work package 5 replaces it with a captured client file and extends the
writer with whatever keys the client needs.

## Legal note

The Steam Subscriber Agreement (revision 2026-09-10) §2.G prohibits emulating
Valve's communication protocols without consent; SteamKit2 and DepotDownloader
have been tolerated for years. The plan records this as a legal risk the user
accepts. This layer acts on the user's own account for the user's own
purchases only and does not bypass DRM.

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
| not yet run | pending | 0.3.0 | 0.3.0 | |

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
| `recv_msg` | `SteamClient<LoggedIn>::recv_msg` (also on `Ready`) | `steamroom-0.3.0/src/client/mod.rs:533` (`:366`) |
| `EMsg::CLIENT_LICENSE_LIST` | `EMsg::CLIENT_LICENSE_LIST = EMsg(780)` | `steamroom-0.3.0/src/messages/mod.rs:27` |
| `CMsgClientLicenseList` | `steamroom::generated::CMsgClientLicenseList` (prost, `build.rs`) | `steamroom-0.3.0/proto/steam/steammessages_clientserver.proto:167`; `src/generated/mod.rs:1` |
| MULTI unpacking | `steamroom::client::multi::unpack_multi`, `messages::header::PacketHeader::parse` | `steamroom-0.3.0/src/client/multi.rs:10`, `src/messages/header.rs:53` |
| CM connect | `CmServer::fetch`/`defaults`, `TcpTransport::connect`, `SteamClient::connect`, `encrypt`, `prepare` | `steamroom-0.3.0/src/connection/mod.rs:42,35`, `src/transport/tcp.rs:21`, `src/client/mod.rs:97,143,276` |
| fixed logon id | `CMsgClientLogon.obfuscated_private_ip: Option<CMsgIpAddress>`, `c_msg_ip_address::Ip::V4(u32)` | generated from `steammessages_base.proto:64` |
| EResult mapping | `steamroom::enums::EResultError::{RateLimitExceeded, LoginDeniedThrottle, TwoFactorRequired, TwoFactorCodeMismatch, InvalidPassword, AccessDenied, Expired, Revoked, Banned}` | `steamroom-0.3.0/src/enums/mod.rs:33-72` |
| `fd-lock` / `rustix` flock | `rustix::fs::flock(fd, FlockOperation::{LockExclusive, NonBlockingLockExclusive, Unlock})` | `rustix-1.1.6/src/fs/fd.rs:322`, `src/backend/libc/fs/types.rs:943` (what `fd-lock-4.0.4/src/sys/unix/rw_lock.rs:20-32` calls) |
| `set_generic_password` | `security_framework::passwords::set_generic_password(service, account, &[u8])` | `security-framework-3.7.0/src/passwords.rs:26` |
| `find_generic_password` | `security_framework::passwords::generic_password(PasswordOptions::new_generic_password(..))` (SecItem API) | `security-framework-3.7.0/src/passwords.rs:53`, `src/passwords_options.rs:63` |
| delete | `security_framework::passwords::delete_generic_password(service, account)` | `security-framework-3.7.0/src/passwords.rs:63` |
| not-found code | `errSecItemNotFound = -25300` | `security-framework-sys-2.17.0/src/base.rs:61` |
| password prompt | `rpassword::prompt_password` | `rpassword-7.5.4/src/lib.rs:270` |
