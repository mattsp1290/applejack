//! The CDN side of a session: chunk and manifest downloads over
//! `steamroom`'s `CdnClient`.
//!
//! This replaces `steamroom_client::download::CdnChunkFetcher` for two
//! reasons. A CDN auth token travels in the request URL and `reqwest::Error`
//! prints the URL, so every error leaves here through [`without_url`]. And a
//! token is per depot and host, which that fetcher cannot express; it is
//! requested the first time a host answers 403.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::json;
use steamroom::cdn::server::CdnServer;
use steamroom::cdn::{CdnClient, CdnServerPool};
use steamroom::depot::manifest::DepotManifest;
use steamroom::depot::{ChunkId, DepotId, ManifestId};
use steamroom::error::Error as SteamError;
use steamroom::generated::CContentServerDirectoryServerInfo;
use steamroom_client::download::{BoxError, ChunkFetcher};
use steamroom_client::manifest::parse_cdn_manifest;

use super::RATE_LIMIT_RETRY_AFTER;
use super::session::Inner;
use crate::auth::AuthError;

/// `CdnClient` sets no timeout of its own.
const CDN_TIMEOUT: Duration = Duration::from_secs(120);

/// The shortest gap between two token requests for one depot and host. A
/// host that answers 403 to a fresh token will not change its mind, and
/// asking per chunk would earn a rate limit.
const TOKEN_REFRESH_FLOOR: Duration = Duration::from_secs(30);

const FORBIDDEN: u16 = 403;
const NOT_FOUND: u16 = 404;
const TOO_MANY_REQUESTS: u16 = 429;
const SERVICE_UNAVAILABLE: u16 = 503;

/// One usable entry of `GetServersForSteamPipe`.
#[derive(Debug, Clone)]
pub(super) struct ServerEntry {
    server: CdnServer,
    allowed_appids: Vec<u32>,
}

/// `CdnServer` is `#[non_exhaustive]` and only `SteamClient<LoggedIn>` builds
/// one, so this goes through its `serde` form.
pub(super) fn cdn_server(info: &CContentServerDirectoryServerInfo) -> Option<ServerEntry> {
    let kind = info.r#type.as_deref()?;
    if kind != "SteamCache" && kind != "CDN" {
        return None;
    }
    let listed = info.host.as_deref()?;
    let https = matches!(
        info.https_support.as_deref(),
        Some("mandatory" | "optional")
    );
    let default_port = if https { 443 } else { 80 };
    let (host, port) = match listed.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().unwrap_or(default_port)),
        None => (listed, default_port),
    };
    let server = serde_json::from_value(json!({
        "host": host,
        "port": port,
        "https": https,
        "vhost": info.vhost.clone().unwrap_or_default(),
    }))
    .ok()?;
    Some(ServerEntry {
        server,
        allowed_appids: info.allowed_app_ids.clone(),
    })
}

/// `CdnServer::build_url` appends a token as is, so it must be a query string.
pub(super) fn token_query(token: String) -> String {
    if token.starts_with('?') {
        token
    } else {
        format!("?{token}")
    }
}

/// Strips the URL, and with it any CDN auth token, from an HTTP error.
pub(super) fn without_url(error: SteamError) -> SteamError {
    match error {
        SteamError::Http(error) => SteamError::Http(error.without_url()),
        other => other,
    }
}

#[derive(Debug, Clone)]
struct CachedToken {
    value: String,
    fetched_at: Instant,
}

/// What to do after a 403.
#[derive(Debug, PartialEq, Eq)]
enum OnForbidden {
    /// Another task already replaced the token that failed.
    RetryWith(String),
    Refresh,
    GiveUp,
}

fn on_forbidden(used: Option<&str>, cached: Option<&CachedToken>) -> OnForbidden {
    match cached {
        Some(token) if used != Some(token.value.as_str()) => {
            OnForbidden::RetryWith(token.value.clone())
        }
        Some(token) if token.fetched_at.elapsed() < TOKEN_REFRESH_FLOOR => OnForbidden::GiveUp,
        _ => OnForbidden::Refresh,
    }
}

/// Encrypted chunks and manifests from the CDN servers Steam listed for one
/// app.
pub struct LiveChunks {
    inner: Arc<Inner>,
    appid: u32,
    cdn: CdnClient,
    pool: CdnServerPool,
    tokens: Mutex<HashMap<(u32, String), CachedToken>>,
}

impl std::fmt::Debug for LiveChunks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveChunks")
            .field("appid", &self.appid)
            .finish_non_exhaustive()
    }
}

impl LiveChunks {
    pub(super) fn new(
        inner: Arc<Inner>,
        appid: u32,
        servers: &[ServerEntry],
    ) -> Result<Self, AuthError> {
        let usable: Vec<CdnServer> = servers
            .iter()
            .filter(|entry| {
                entry.allowed_appids.is_empty() || entry.allowed_appids.contains(&appid)
            })
            .map(|entry| entry.server.clone())
            .collect();
        if usable.is_empty() {
            return Err(AuthError::Connection(format!(
                "Steam listed no CDN server for app {appid}"
            )));
        }
        Ok(Self {
            inner,
            appid,
            cdn: CdnClient::new().map_err(|error| {
                AuthError::Connection(format!("CDN client: {}", without_url(error)))
            })?,
            pool: CdnServerPool::new(usable),
            tokens: Mutex::new(HashMap::new()),
        })
    }

    fn cached_token(&self, depot_id: u32, host: &str) -> Option<CachedToken> {
        self.tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(depot_id, host.to_owned()))
            .cloned()
    }

    /// Runs `fetch`, and on a 403 once more with a newer token, which also
    /// covers a cached token that expired.
    async fn authorized<F, Fut>(
        &self,
        server: &CdnServer,
        depot_id: u32,
        fetch: F,
    ) -> Result<Bytes, SteamError>
    where
        F: Fn(Option<String>) -> Fut + Send,
        Fut: Future<Output = Result<Bytes, SteamError>> + Send,
    {
        let used = self
            .cached_token(depot_id, &server.host)
            .map(|token| token.value);
        let outcome = bounded(fetch(used.clone())).await;
        let is_forbidden = matches!(
            &outcome,
            Err(SteamError::CdnStatus { status, .. }) if status.as_u16() == FORBIDDEN
        );
        if !is_forbidden {
            return outcome;
        }
        let cached = self.cached_token(depot_id, &server.host);
        let token = match on_forbidden(used.as_deref(), cached.as_ref()) {
            OnForbidden::RetryWith(token) => token,
            OnForbidden::GiveUp => return outcome,
            OnForbidden::Refresh => {
                let Ok(token) = self
                    .inner
                    .cdn_auth_token(self.appid, depot_id, &server.host)
                    .await
                else {
                    return outcome;
                };
                self.tokens
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(
                        (depot_id, server.host.clone()),
                        CachedToken {
                            value: token.clone(),
                            fetched_at: Instant::now(),
                        },
                    );
                token
            }
        };
        bounded(fetch(Some(token))).await
    }

    /// Fetches and parses a manifest, rotating through the pool. `Ok(None)`
    /// when every server answered 404.
    pub(super) async fn manifest(
        &self,
        depot_id: u32,
        manifest_id: u64,
        request_code: u64,
    ) -> Result<Option<DepotManifest>, AuthError> {
        let mut not_found = 0;
        let mut last_error = None;
        for _ in 0..self.pool.len() {
            let (server, wait) = self.pool.pick();
            tokio::time::sleep(wait).await;
            let fetched = self
                .authorized(server, depot_id, |token| async move {
                    self.cdn
                        .download_manifest(
                            server,
                            DepotId(depot_id),
                            ManifestId(manifest_id),
                            request_code,
                            token.as_deref(),
                        )
                        .await
                })
                .await;
            match fetched {
                Ok(data) => {
                    self.pool.report_success(server);
                    return parse_cdn_manifest(&data).map(Some).map_err(|error| {
                        AuthError::Connection(format!("depot {depot_id} manifest: {error}"))
                    });
                }
                Err(error) => {
                    let retry_after = match &error {
                        SteamError::CdnStatus {
                            status,
                            retry_after,
                        } => {
                            match status.as_u16() {
                                NOT_FOUND => not_found += 1,
                                TOO_MANY_REQUESTS | SERVICE_UNAVAILABLE => {
                                    return Err(AuthError::RateLimited {
                                        retry_after: retry_after
                                            .map_or(RATE_LIMIT_RETRY_AFTER, Duration::from_secs),
                                    });
                                }
                                _ => {}
                            }
                            retry_after.map(Duration::from_secs)
                        }
                        _ => None,
                    };
                    self.pool.report_failure(server, retry_after);
                    last_error = Some(without_url(error));
                }
            }
        }
        if not_found == self.pool.len() {
            return Ok(None);
        }
        Err(AuthError::Connection(format!(
            "no CDN server returned depot {depot_id} manifest {manifest_id}: {}",
            last_error.map_or_else(|| "empty pool".to_owned(), |error| error.to_string())
        )))
    }
}

async fn bounded(
    fetch: impl Future<Output = Result<Bytes, SteamError>>,
) -> Result<Bytes, SteamError> {
    tokio::time::timeout(CDN_TIMEOUT, fetch)
        .await
        .unwrap_or_else(|_| {
            Err(SteamError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "CDN request timed out",
            )))
        })
}

impl ChunkFetcher for LiveChunks {
    async fn fetch_chunk(&self, depot_id: DepotId, chunk_id: &ChunkId) -> Result<Bytes, BoxError> {
        let (server, wait) = self.pool.pick();
        tokio::time::sleep(wait).await;
        let fetched = self
            .authorized(server, depot_id.0, |token| async move {
                self.cdn
                    .download_chunk(server, depot_id, chunk_id, token.as_deref())
                    .await
            })
            .await;
        match fetched {
            Ok(data) => {
                self.pool.report_success(server);
                Ok(data)
            }
            Err(error) => {
                let retry_after = match &error {
                    SteamError::CdnStatus { retry_after, .. } => {
                        retry_after.map(Duration::from_secs)
                    }
                    _ => None,
                };
                self.pool.report_failure(server, retry_after);
                Err(Box::new(without_url(error)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refused connection on a loopback port that was just released: no
    /// packet leaves the machine, and `reqwest` reports the URL it tried.
    #[tokio::test]
    async fn an_http_error_loses_the_token_in_its_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let server: CdnServer = serde_json::from_value(json!({
            "host": "127.0.0.1",
            "port": port,
            "https": false,
            "vhost": "fixture",
        }))
        .unwrap();
        let token = "?token=fixture-cdn-token-not-a-real-value";

        let error = CdnClient::new()
            .unwrap()
            .download_chunk(&server, DepotId(1), &ChunkId([0; 20]), Some(token))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("fixture-cdn-token"), "{error}");

        let cleaned = without_url(error);
        assert!(!format!("{cleaned} {cleaned:?}").contains("fixture-cdn-token"));
    }

    #[test]
    fn a_403_asks_steam_for_a_token_only_when_that_can_help() {
        let fresh = CachedToken {
            value: "?new".to_owned(),
            fetched_at: Instant::now(),
        };
        assert_eq!(on_forbidden(None, None), OnForbidden::Refresh);
        assert_eq!(
            on_forbidden(Some("?old"), Some(&fresh)),
            OnForbidden::RetryWith("?new".to_owned())
        );
        assert_eq!(
            on_forbidden(None, Some(&fresh)),
            OnForbidden::RetryWith("?new".to_owned())
        );
        assert_eq!(
            on_forbidden(Some("?new"), Some(&fresh)),
            OnForbidden::GiveUp
        );
        if let Some(fetched_at) = Instant::now().checked_sub(TOKEN_REFRESH_FLOOR) {
            let expired = CachedToken {
                value: "?new".to_owned(),
                fetched_at,
            };
            assert_eq!(
                on_forbidden(Some("?new"), Some(&expired)),
                OnForbidden::Refresh
            );
        }
    }

    #[test]
    fn a_token_becomes_a_query_string_once() {
        assert_eq!(token_query("token=x".to_owned()), "?token=x");
        assert_eq!(token_query("?token=x".to_owned()), "?token=x");
    }

    #[test]
    fn only_content_servers_with_a_host_are_used() {
        let info =
            |kind: &str, host: Option<&str>, https: &str| CContentServerDirectoryServerInfo {
                r#type: Some(kind.to_owned()),
                host: host.map(str::to_owned),
                vhost: Some("cdn.fixture".to_owned()),
                https_support: Some(https.to_owned()),
                ..Default::default()
            };
        let cdn = cdn_server(&info("CDN", Some("cdn.fixture:8080"), "unavailable")).unwrap();
        assert_eq!(cdn.server.host, "cdn.fixture");
        assert_eq!(cdn.server.port, 8080);
        assert!(!cdn.server.https);
        assert_eq!(
            cdn_server(&info("SteamCache", Some("cache.fixture"), "mandatory"))
                .unwrap()
                .server
                .port,
            443
        );
        assert!(cdn_server(&info("OpenCache", Some("x"), "optional")).is_none());
        assert!(cdn_server(&info("CDN", None, "optional")).is_none());
    }
}
