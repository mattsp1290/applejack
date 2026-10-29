//! The login driver against a scripted fake connection.

use std::future::{Future, ready};

use applejack_steam::Error;
use applejack_steam::auth::{
    AccessToken, AuthError, AuthMode, Authenticator, Credentials, GuardFactor, License, LoginStep,
    Password, SteamConnection, Tokens,
};
use applejack_steam::keychain::{InMemoryStore, RefreshToken, RefreshTokenStore};

const FIXTURE_TOKEN: &str = "fixture-refresh-token-not-a-real-value";

#[derive(Default)]
struct Fake {
    guard: Option<GuardFactor>,
    calls: Vec<String>,
    logged_on_with: Option<(String, String)>,
}

impl SteamConnection for Fake {
    fn begin_qr(&mut self) -> impl Future<Output = Result<String, AuthError>> + Send {
        self.calls.push("begin_qr".into());
        ready(Ok("https://s.team/q/fixture".to_owned()))
    }

    fn begin_credentials(
        &mut self,
        credentials: &Credentials,
    ) -> impl Future<Output = Result<Option<GuardFactor>, AuthError>> + Send {
        self.calls
            .push(format!("begin_credentials {}", credentials.account_name));
        ready(Ok(self.guard))
    }

    fn submit_guard_code(
        &mut self,
        code: &str,
    ) -> impl Future<Output = Result<(), AuthError>> + Send {
        self.calls.push(format!("submit_guard_code {code}"));
        ready(if code == "BAD" {
            Err(AuthError::InvalidGuardCode)
        } else {
            Ok(())
        })
    }

    fn wait_for_tokens(&mut self) -> impl Future<Output = Result<Tokens, AuthError>> + Send {
        self.calls.push("wait_for_tokens".into());
        ready(Ok(Tokens {
            account_name: "redacted".to_owned(),
            refresh_token: RefreshToken::new(FIXTURE_TOKEN),
            access_token: AccessToken::new("access"),
        }))
    }

    fn logon(
        &mut self,
        account_name: &str,
        refresh_token: &RefreshToken,
    ) -> impl Future<Output = Result<(), AuthError>> + Send {
        self.calls.push("logon".into());
        self.logged_on_with = Some((account_name.to_owned(), refresh_token.expose().to_owned()));
        ready(Ok(()))
    }

    fn license_list(&mut self) -> impl Future<Output = Result<Vec<License>, AuthError>> + Send {
        ready(Ok(vec![License {
            package_id: 0,
            license_type: 1,
            flags: 0,
            owner_id: 0,
            access_token: 0,
        }]))
    }
}

fn credentials() -> Credentials {
    Credentials {
        account_name: "redacted".to_owned(),
        password: Password::new("not-a-real-password"),
    }
}

#[tokio::test]
async fn qr_login_stores_the_refresh_token() {
    let store = InMemoryStore::default();
    let mut fake = Fake::default();
    let mut auth = Authenticator::new(&mut fake, &store);
    let step = auth.begin(AuthMode::Qr, None).await.unwrap();
    assert_eq!(
        step,
        LoginStep::QrReady {
            payload: "https://s.team/q/fixture".to_owned()
        }
    );
    assert!(store.get().unwrap().is_none());
    let step = auth.finish().await.unwrap();
    assert_eq!(
        step,
        LoginStep::Done {
            account_name: "redacted".to_owned()
        }
    );
    assert_eq!(
        store.get().unwrap().as_ref().map(RefreshToken::expose),
        Some(FIXTURE_TOKEN)
    );
    assert_eq!(fake.calls, ["begin_qr", "wait_for_tokens"]);
}

#[tokio::test]
async fn credentials_login_without_guard_completes_immediately() {
    let store = InMemoryStore::default();
    let mut fake = Fake::default();
    let mut auth = Authenticator::new(&mut fake, &store);
    let step = auth
        .begin(AuthMode::Credentials, Some(&credentials()))
        .await
        .unwrap();
    assert!(matches!(step, LoginStep::Done { .. }));
    assert!(store.get().unwrap().is_some());
}

#[tokio::test]
async fn credentials_login_with_guard_code() {
    let store = InMemoryStore::default();
    let mut fake = Fake {
        guard: Some(GuardFactor::DeviceCode),
        ..Fake::default()
    };
    let mut auth = Authenticator::new(&mut fake, &store);
    let step = auth
        .begin(AuthMode::Credentials, Some(&credentials()))
        .await
        .unwrap();
    assert_eq!(
        step,
        LoginStep::GuardRequired {
            factor: GuardFactor::DeviceCode
        }
    );

    let error = auth.finish().await.unwrap_err();
    assert!(matches!(
        error,
        Error::Auth(AuthError::GuardRequired {
            factor: GuardFactor::DeviceCode
        })
    ));

    let error = auth.submit_guard_code("BAD").await.unwrap_err();
    assert!(matches!(error, Error::Auth(AuthError::InvalidGuardCode)));
    assert!(store.get().unwrap().is_none());

    let step = auth.submit_guard_code("GOOD1").await.unwrap();
    assert!(matches!(step, LoginStep::Done { .. }));
    assert!(store.get().unwrap().is_some());
    assert_eq!(
        fake.calls,
        [
            "begin_credentials redacted",
            "submit_guard_code BAD",
            "submit_guard_code GOOD1",
            "wait_for_tokens"
        ]
    );
}

#[tokio::test]
async fn device_confirmation_finishes_without_a_code() {
    let store = InMemoryStore::default();
    let mut fake = Fake {
        guard: Some(GuardFactor::DeviceConfirmation),
        ..Fake::default()
    };
    let mut auth = Authenticator::new(&mut fake, &store);
    let step = auth
        .begin(AuthMode::Credentials, Some(&credentials()))
        .await
        .unwrap();
    assert_eq!(
        step,
        LoginStep::GuardRequired {
            factor: GuardFactor::DeviceConfirmation
        }
    );
    let error = auth.submit_guard_code("12345").await.unwrap_err();
    assert!(matches!(error, Error::Auth(AuthError::OutOfOrder(_))));
    assert!(matches!(
        auth.finish().await.unwrap(),
        LoginStep::Done { .. }
    ));
}

#[tokio::test]
async fn out_of_order_steps_are_rejected() {
    let store = InMemoryStore::default();
    let mut fake = Fake::default();
    let mut auth = Authenticator::new(&mut fake, &store);
    assert!(matches!(
        auth.finish().await.unwrap_err(),
        Error::Auth(AuthError::OutOfOrder(_))
    ));
    assert!(matches!(
        auth.begin(AuthMode::Credentials, None).await.unwrap_err(),
        Error::Auth(AuthError::OutOfOrder(_))
    ));
    assert_eq!(fake.calls, Vec::<String>::new());
}

#[tokio::test]
async fn refresh_uses_the_stored_token_and_logout_deletes_it() {
    let store = InMemoryStore::default();
    let mut fake = Fake::default();
    let mut auth = Authenticator::new(&mut fake, &store);
    assert!(matches!(
        auth.refresh("redacted").await.unwrap_err(),
        Error::Auth(AuthError::NoStoredToken)
    ));

    store.put(&RefreshToken::new(FIXTURE_TOKEN)).unwrap();
    auth.refresh("redacted").await.unwrap();
    auth.logout().unwrap();
    assert!(store.get().unwrap().is_none());
    assert_eq!(
        fake.logged_on_with,
        Some(("redacted".to_owned(), FIXTURE_TOKEN.to_owned()))
    );
    assert_eq!(fake.license_list().await.unwrap().len(), 1);
}

#[test]
fn secrets_never_print_their_values() {
    let tokens = Tokens {
        account_name: "redacted".to_owned(),
        refresh_token: RefreshToken::new(FIXTURE_TOKEN),
        access_token: AccessToken::new("access-fixture"),
    };
    let debug = format!("{tokens:?}");
    assert!(!debug.contains(FIXTURE_TOKEN));
    assert!(!debug.contains("access-fixture"));
    assert!(!format!("{:?}", credentials()).contains("not-a-real-password"));
    let license = License {
        package_id: 1,
        license_type: 1,
        flags: 0,
        owner_id: 0,
        access_token: 0xDEAD_BEEF_CAFE,
    };
    let debug = format!("{license:?}");
    assert!(debug.contains("package_id: 1"));
    assert!(!debug.contains("access_token"));
    assert!(!debug.contains("DEAD_BEEF_CAFE"), "{debug}");
    assert!(
        !debug
            .to_lowercase()
            .contains(&format!("{:x}", 0xDEAD_BEEF_CAFEu64))
    );
    assert!(!debug.contains(&0xDEAD_BEEF_CAFEu64.to_string()));
}
