//! One contract for every `RefreshTokenStore`. `InMemoryStore` always runs;
//! the Security framework store runs only under `APPLEJACK_LIVE_STEAM=1`
//! against a test-only item, never the real `steam-refresh-token` item.

use applejack_steam::keychain::{
    InMemoryStore, RefreshToken, RefreshTokenStore, SecurityFrameworkStore,
};

const FIXTURE_TOKEN: &str = "fixture-refresh-token-not-a-real-value";

fn exercise(store: &dyn RefreshTokenStore) {
    store.delete().unwrap();
    assert_eq!(store.get().unwrap(), None);

    store.put(&RefreshToken::new(FIXTURE_TOKEN)).unwrap();
    assert_eq!(
        store.get().unwrap().as_ref().map(RefreshToken::expose),
        Some(FIXTURE_TOKEN)
    );

    store.put(&RefreshToken::new("second")).unwrap();
    assert_eq!(
        store.get().unwrap().as_ref().map(RefreshToken::expose),
        Some("second")
    );

    store.delete().unwrap();
    assert_eq!(store.get().unwrap(), None);
    store.delete().unwrap();
}

#[test]
fn in_memory_store_honors_the_contract() {
    exercise(&InMemoryStore::default());
}

#[test]
fn security_framework_store_honors_the_contract_when_live() {
    if std::env::var("APPLEJACK_LIVE_STEAM").as_deref() != Ok("1") {
        eprintln!("skipped: set APPLEJACK_LIVE_STEAM=1 to exercise the Keychain");
        return;
    }
    exercise(&SecurityFrameworkStore::with_names(
        "applejack-test",
        "steam-refresh-token-test",
    ));
}

#[test]
fn refresh_token_never_prints_its_value() {
    let token = RefreshToken::new(FIXTURE_TOKEN);
    assert!(!format!("{token:?}").contains(FIXTURE_TOKEN));
    assert!(!format!("{token}").contains(FIXTURE_TOKEN));
    assert_eq!(token.expose(), FIXTURE_TOKEN);
}
