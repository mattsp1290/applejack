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
  and wait for the mobile approval).
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
  `std::process::Command`; `tests/no_token_argv.rs` greps `src/` and
  `examples/` for one on every test run.
- Fixtures and docs contain no credential values. The fixture token in tests is
  the literal `fixture-refresh-token-not-a-real-value`.
- The `applejack` CLI and the copy embedded in the app are the same signed
  binary with identifier `com.applejack.core`, so both read the item without a
  prompt. An ad-hoc development build may prompt; that is acceptable.

## Locks

`lock::Lock` takes an exclusive `flock` on `<data>/locks/<name>.lock` and
writes one JSON line `{"pid", "command", "acquired_at"}` into it. A
`NonBlocking` acquire on a held lock fails with `Error::LockHeld { pid,
command }` read from that line; a `Blocking` acquire waits.

Lock order is per-appid first, then the library, and the types enforce it:
`LockDir::app(appid, mode)` yields an `AppLock`, and only
`AppLock::library(mode)` yields a `LibraryLock`, which borrows the `AppLock`
and so cannot outlive it. There is no other constructor.

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

`vdf::Document` reads Valve's text KeyValues (quoted and bare tokens, nested
braces, `//` comments, `[$CONDITION]` suffixes, escaped quotes, duplicate keys
preserved in order) and writes Valve's layout, so a client-written file
round-trips byte for byte. `acf::AppManifest` reads and `acf::AcfWriter`
writes `appmanifest_<appid>.acf` with `appid`, `name`, `StateFlags`,
`installdir`, `LastUpdated`, `SizeOnDisk`, `buildid`, and `InstalledDepots`
(`manifest`, `size` per depot). The field names follow the public format as
documented by SteamKit and SteamDB. `tests/fixtures/appmanifest_000000.acf` is
hand-written and redacted (appid 0); work package 5 replaces it with a captured
client file and extends the writer with whatever keys the client needs.

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

QR mode prints the challenge URL; render it with any QR tool and scan it with
the Steam mobile app. Credentials mode reads the account name from stdin and
the password from the terminal without echo (`rpassword`). The spike stores the
refresh token in an `InMemoryStore`, logs on again with it, decodes the license
list, prints the license count and crate versions, and never prints the token.

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
| tokens after approval | `ApprovedAuth::tokens` → `steamroom::auth::AuthTokens { access_token, refresh_token, account_name }` | `steamroom-client-0.3.0/src/login/terminal.rs:83`, `steamroom-0.3.0/src/auth/mod.rs:22` |
| refresh-token logon | `SteamClient<Ready>::login(ClientMsg)` with `CMsgClientLogon { account_name, access_token, obfuscated_private_ip, .. }` | `steamroom-0.3.0/src/client/mod.rs:286`; proto `steammessages_clientserver_login.proto` (`account_name`=50, `access_token`=108, `obfuscated_private_ip`=11) |
| (reference) | `TokenLogin::login` builds the same message without a logon id | `steamroom-client-0.3.0/src/login/terminal.rs:55` |
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
| `find_generic_password` | `security_framework::passwords::generic_password(PasswordOptions::new_generic_password(..))` (SecItem API) | `security-framework-3.7.0/src/passwords.rs:51`, `src/passwords_options.rs:63` |
| delete | `security_framework::passwords::delete_generic_password(service, account)` | `security-framework-3.7.0/src/passwords.rs:63` |
| not-found code | `errSecItemNotFound = -25300` | `security-framework-sys-2.17.0/src/base.rs:61` |
| password prompt | `rpassword::prompt_password` | `rpassword-7.5.4/src/lib.rs:270` |
