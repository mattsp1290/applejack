//! Steam from the core's side: the sign-in, the library cache, and what ties
//! them to a host. The CLI and the RPC handler call these and nothing else,
//! so they cannot drift. Everything that reaches Steam goes through a
//! [`SteamHost`]; tests supply a scripted one and never touch the Keychain.
//!
//! A token is read through the store and handed to the connection, and goes
//! no further: not into a row, a log, a report, or a child process.

use std::future::{Future, ready};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use applejack_engine::rosetta_wine::utc_now;
use applejack_steam::live::{SteamroomConnection, SteamroomSession};
use applejack_steam::{
    AuthError, AuthPoll, Authenticator, Error as SteamError, InMemoryStore, Library, Lock, LockDir,
    LoginState, Mode, OwnedApp, RefreshToken, RefreshTokenStore, SecurityFrameworkStore,
    SteamConnection,
};
use serde::{Deserialize, Serialize};
use tokio::runtime::{Builder, Runtime};

use crate::config::{AppRow, Drm, LibraryRow};
use crate::error::CoreError;
use crate::rpc::jobs::JobId;
use crate::run::printable;
use crate::state::Core;

pub use applejack_steam::{AuthMode, Credentials, GuardFactor, LoginStep, Password};

/// `meta` key of the account a token logon names. Not a secret, and never
/// the token.
pub const META_ACCOUNT: &str = "steam_account_name";

/// How long a login stays at [`LoginStep::Waiting`]. Steam issues tokens
/// within a poll or two of a code; no response seen says how long, and
/// nothing else would end the wait.
const LOGIN_WAIT_LIMIT: Duration = Duration::from_secs(120);

/// Where the core gets its Steam connections and its token store.
pub trait SteamHost: Send + Sync + 'static {
    type Connection: SteamConnection + Send + 'static;
    type Store: RefreshTokenStore + Send + Sync;

    fn connection(&self) -> Self::Connection;

    fn store(&self) -> &Self::Store;
}

/// Steam itself and the macOS Keychain.
#[derive(Debug, Default)]
pub struct LiveSteam {
    store: SecurityFrameworkStore,
}

impl SteamHost for LiveSteam {
    type Connection = SteamroomConnection;
    type Store = SecurityFrameworkStore;

    fn connection(&self) -> SteamroomConnection {
        SteamroomConnection::new()
    }

    fn store(&self) -> &SecurityFrameworkStore {
        &self.store
    }
}

/// A host with no Steam: every call fails and no Keychain is near it. This
/// is what a handler built without [`LiveSteam`] gets, so a test that walks
/// the method table cannot reach a real account.
#[derive(Debug, Default)]
pub struct NoSteam {
    store: InMemoryStore,
}

/// [`NoSteam`]'s connection.
#[derive(Debug)]
pub struct NoConnection;

fn no_steam<T>() -> Result<T, AuthError> {
    Err(AuthError::Connection(
        "this core has no Steam connection".to_owned(),
    ))
}

impl SteamConnection for NoConnection {
    type Session = SteamroomSession;

    fn begin_qr(&mut self) -> impl Future<Output = Result<String, AuthError>> + Send {
        ready(no_steam())
    }

    fn begin_credentials(
        &mut self,
        _credentials: &Credentials,
    ) -> impl Future<Output = Result<Option<GuardFactor>, AuthError>> + Send {
        ready(no_steam())
    }

    fn submit_guard_code(
        &mut self,
        _code: &str,
    ) -> impl Future<Output = Result<(), AuthError>> + Send {
        ready(no_steam())
    }

    fn poll_tokens(&mut self) -> impl Future<Output = Result<AuthPoll, AuthError>> + Send {
        ready(no_steam())
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn logon(
        &mut self,
        _account_name: &str,
        _refresh_token: &RefreshToken,
    ) -> impl Future<Output = Result<SteamroomSession, AuthError>> + Send {
        ready(no_steam())
    }
}

impl SteamHost for NoSteam {
    type Connection = NoConnection;
    type Store = InMemoryStore;

    fn connection(&self) -> NoConnection {
        NoConnection
    }

    fn store(&self) -> &InMemoryStore {
        &self.store
    }
}

/// One title in `steam library` and `steam.library`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryEntry {
    pub appid: u32,
    pub name: String,
    pub installdir: String,
    pub size_bytes: u64,
    pub drm: Drm,
    pub default_branch: String,
    pub install_state: InstallState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallState {
    Installed,
    NotInstalled,
    UpdateAvailable,
}

/// The cached library. A request with `refresh` answers with the cache as it
/// stands and `job_id`; the fetch can outlast a request, so its end arrives
/// as `job.done` and the next read has the new rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteamLibraryResult {
    pub apps: Vec<LibraryEntry>,
    pub fetched_at: Option<String>,
    pub job_id: Option<JobId>,
}

/// Steam's calls wait on timers and tasks, and the core is otherwise
/// synchronous, so each piece of Steam work brings its own runtime.
pub(crate) fn steam_runtime() -> Result<Runtime, CoreError> {
    Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| CoreError::Message(format!("cannot start the Steam runtime: {error}")))
}

/// A silent refresh that no login answers is [`CoreError::SteamLoginRequired`].
fn refresh_failure(error: SteamError) -> CoreError {
    match error {
        SteamError::Auth(AuthError::NoStoredToken) => CoreError::SteamLoginRequired {
            reason: "no stored token".to_owned(),
        },
        SteamError::Auth(AuthError::Refused) => CoreError::SteamLoginRequired {
            reason: "Steam refused the stored token".to_owned(),
        },
        other => CoreError::SteamRefreshFailed(other),
    }
}

/// A logged-in session and the lock that makes it the only one. The session
/// goes first: the fields drop in this order.
pub(crate) struct SignedIn<S> {
    pub(crate) session: S,
    _lock: Lock,
}

/// Opens a session from the stored token, holding `steam-session` for as long
/// as it lives. A second holder fails at once and names the first: waiting
/// could be hours behind a download.
pub(crate) async fn silent_logon<H: SteamHost>(
    host: &H,
    data_dir: &Path,
    account: &str,
) -> Result<SignedIn<<H::Connection as SteamConnection>::Session>, CoreError> {
    let lock = LockDir::new(data_dir, "steam").steam_session(Mode::NonBlocking)?;
    let mut connection = host.connection();
    let session = Authenticator::new(&mut connection, host.store())
        .refresh(account)
        .await
        .map_err(refresh_failure)?;
    Ok(SignedIn {
        session,
        _lock: lock,
    })
}

/// One login, from `begin` to [`LoginStep::Done`], across as many calls as it
/// takes. The CLI drives it from a loop and `steam.login.*` from requests;
/// neither blocks on Steam for longer than one call. A code and a mobile
/// approval both finish it: each [`advance`](Self::advance) is one poll or
/// one submit on the same connection, so they cannot race on the wire.
pub struct LoginFlow<H: SteamHost> {
    host: Arc<H>,
    runtime: Runtime,
    connection: H::Connection,
    state: LoginState,
    step: LoginStep,
    polled_at: Instant,
    waiting_since: Option<Instant>,
    wait_limit: Duration,
}

impl<H: SteamHost> std::fmt::Debug for LoginFlow<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginFlow")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl<H: SteamHost> LoginFlow<H> {
    /// Connects and starts the login.
    ///
    /// # Errors
    ///
    /// The Steam failure, or the runtime's.
    pub fn start(
        core: &mut Core,
        host: Arc<H>,
        mode: AuthMode,
        credentials: Option<&Credentials>,
    ) -> Result<Self, CoreError> {
        let runtime = steam_runtime()?;
        let mut connection = host.connection();
        let (step, state) = {
            let mut auth = Authenticator::new(&mut connection, host.store());
            let step = runtime.block_on(auth.begin(mode, credentials))?;
            (step, auth.state())
        };
        let waiting_since = (step == LoginStep::Waiting).then(Instant::now);
        let flow = Self {
            host,
            runtime,
            connection,
            state,
            step,
            polled_at: Instant::now(),
            waiting_since,
            wait_limit: LOGIN_WAIT_LIMIT,
        };
        flow.remember_account(core)?;
        Ok(flow)
    }

    /// Replaces how long the login may stay at [`LoginStep::Waiting`].
    #[must_use]
    pub const fn with_wait_limit(mut self, limit: Duration) -> Self {
        self.wait_limit = limit;
        self
    }

    #[must_use]
    pub const fn step(&self) -> &LoginStep {
        &self.step
    }

    /// How long to wait between polls.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        self.connection.poll_interval()
    }

    /// Submits `code`, or looks at the login, at most once per
    /// [`poll_interval`](Self::poll_interval): a caller that asks sooner
    /// gets the step it already has. A code that arrives once none is
    /// awaited is dropped and the look happens anyway: the mobile approval
    /// won, and that is a success.
    ///
    /// # Errors
    ///
    /// [`AuthError::InvalidGuardCode`] leaves the login open for another
    /// code. Any other error ends it, as does
    /// [`CoreError::SteamLoginStalled`] once [`LoginStep::Waiting`] has
    /// lasted its limit.
    pub fn advance(
        &mut self,
        owner: &mut Core,
        typed: Option<&str>,
    ) -> Result<LoginStep, CoreError> {
        if matches!(self.step, LoginStep::Done { .. }) {
            return Ok(self.step.clone());
        }
        if self
            .waiting_since
            .is_some_and(|since| since.elapsed() >= self.wait_limit)
        {
            self.state = LoginState::Idle;
            return Err(CoreError::SteamLoginStalled {
                seconds: self.wait_limit.as_secs(),
            });
        }
        let wanted = typed
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .filter(|_| matches!(self.state, LoginState::AwaitingGuardCode(_)));
        if wanted.is_none() && self.polled_at.elapsed() < self.poll_interval() {
            return Ok(self.step.clone());
        }
        let mut auth = Authenticator::resume(&mut self.connection, self.host.store(), self.state);
        let outcome = self.runtime.block_on(async {
            match wanted {
                Some(text) => auth.submit_guard_code(text).await,
                None => auth.poll().await,
            }
        });
        self.state = auth.state();
        self.polled_at = Instant::now();
        let step = outcome?;
        let keeps_prompt = matches!(
            self.step,
            LoginStep::QrReady { .. }
                | LoginStep::GuardRequired {
                    factor: GuardFactor::DeviceConfirmation
                }
        );
        if step != LoginStep::Waiting || !keeps_prompt {
            self.step = step;
        }
        self.waiting_since = if self.step == LoginStep::Waiting {
            self.waiting_since.or_else(|| Some(Instant::now()))
        } else {
            None
        };
        self.remember_account(owner)?;
        Ok(self.step.clone())
    }

    /// A different account owns a different library, so its cache goes.
    fn remember_account(&self, core: &mut Core) -> Result<(), CoreError> {
        let LoginStep::Done { account_name } = &self.step else {
            return Ok(());
        };
        let db = core.db()?;
        if db.meta_get(META_ACCOUNT)?.as_deref() != Some(account_name) {
            db.clear_library_cache()?;
        }
        db.meta_set(META_ACCOUNT, account_name)?;
        Ok(())
    }
}

fn entry(row: AppRow) -> LibraryEntry {
    // `latest_buildid` is the default branch's, which says nothing of another.
    let comparable = row.installed_branch.is_none() || row.installed_branch == row.default_branch;
    let install_state = match (row.last_installed_buildid, row.latest_buildid) {
        (None, _) => InstallState::NotInstalled,
        (Some(have), Some(latest)) if comparable && have != latest => InstallState::UpdateAvailable,
        (Some(_), _) => InstallState::Installed,
    };
    LibraryEntry {
        appid: row.appid,
        name: row.name.unwrap_or_default(),
        installdir: row.installdir.unwrap_or_default(),
        size_bytes: row.size_bytes.unwrap_or(0),
        drm: row.drm,
        default_branch: row.default_branch.unwrap_or_default(),
        install_state,
    }
}

fn library_row(app: &OwnedApp) -> LibraryRow {
    LibraryRow {
        appid: app.appid,
        name: printable(&app.name),
        installdir: app.installdir.clone(),
        size_bytes: app.size,
        default_branch: app.default_branch.clone(),
        latest_buildid: app.builds.get(&app.default_branch).copied(),
    }
}

impl Core {
    /// The account a silent logon names.
    pub(crate) fn steam_account(&mut self) -> Result<String, CoreError> {
        self.reading(|db| Ok(db.meta_get(META_ACCOUNT)?))?
            .ok_or_else(|| CoreError::SteamLoginRequired {
                reason: "no account on record".to_owned(),
            })
    }

    /// The library cache. Touches nothing but the database.
    ///
    /// # Errors
    ///
    /// The database failure.
    pub fn steam_library_cached(&mut self) -> Result<SteamLibraryResult, CoreError> {
        let rows = self.reading(|db| Ok(db.cached_library()?))?;
        let fetched_at = rows.iter().find_map(|row| row.fetched_at.clone());
        Ok(SteamLibraryResult {
            apps: rows.into_iter().map(entry).collect(),
            fetched_at,
            job_id: None,
        })
    }

    /// Reads the library from Steam and replaces the cache.
    ///
    /// # Errors
    ///
    /// [`CoreError::SteamLoginRequired`] when nothing can sign in, or when
    /// the account signed out while the fetch ran, which drops the rows; and
    /// the Steam or database failure.
    pub fn steam_library_refresh<H: SteamHost>(
        &mut self,
        host: &H,
    ) -> Result<SteamLibraryResult, CoreError> {
        let account = self.steam_account()?;
        let runtime = steam_runtime()?;
        let data_dir = self.data_dir().to_path_buf();
        let library = runtime.block_on(async {
            let signed_in = silent_logon(host, &data_dir, &account).await?;
            Ok::<_, CoreError>(Library::fetch(&signed_in.session).await?)
        })?;
        let rows: Vec<LibraryRow> = library.windows_apps().map(library_row).collect();
        let kept = self
            .db()?
            .cache_library_for(META_ACCOUNT, &account, &rows, &utc_now())?;
        if !kept {
            return Err(CoreError::SteamLoginRequired {
                reason: "the account signed out during the refresh".to_owned(),
            });
        }
        self.steam_library_cached()
    }

    /// Deletes the token, the cached library, and the account name. Titles
    /// on disk stay and go on running.
    ///
    /// # Errors
    ///
    /// The Keychain or database failure. A Keychain that refuses leaves the
    /// cache, so the failure can be retried from the same state.
    pub fn steam_logout<H: SteamHost>(&mut self, host: &H) -> Result<(), CoreError> {
        host.store().delete()?;
        self.reading(|db| Ok(db.forget_account(META_ACCOUNT)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(have: Option<u64>, latest: Option<u64>) -> AppRow {
        AppRow {
            last_installed_buildid: have,
            latest_buildid: latest,
            ..AppRow::default()
        }
    }

    #[test]
    fn the_install_state_comes_from_the_build_ids() {
        assert_eq!(
            entry(row(None, Some(2))).install_state,
            InstallState::NotInstalled
        );
        assert_eq!(
            entry(row(Some(2), Some(2))).install_state,
            InstallState::Installed
        );
        assert_eq!(
            entry(row(Some(1), Some(2))).install_state,
            InstallState::UpdateAvailable
        );
        assert_eq!(
            entry(row(Some(1), None)).install_state,
            InstallState::Installed
        );
    }
}
