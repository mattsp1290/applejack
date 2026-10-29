//! Refresh-token storage. The Security framework store writes a generic
//! password item through `SecItemAdd`/`SecItemCopyMatching`/`SecItemDelete`
//! (never the `security` command line tool) with service `applejack` and
//! account `steam-refresh-token`.

use std::fmt;
use std::sync::{Mutex, PoisonError};

use security_framework::passwords::{
    PasswordOptions, delete_generic_password, generic_password, set_generic_password,
};

use crate::Error;

/// Keychain service name of the refresh-token item.
pub const SERVICE: &str = "applejack";
/// Keychain account name of the refresh-token item.
pub const ACCOUNT: &str = "steam-refresh-token";

/// `errSecItemNotFound` from `security-framework-sys/src/base.rs`.
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

/// A Steam refresh token. `Debug` and `Display` never reveal the value; call
/// [`RefreshToken::expose`] to read it.
#[derive(Clone, PartialEq, Eq)]
pub struct RefreshToken(String);

impl RefreshToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RefreshToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RefreshToken(<redacted>)")
    }
}

impl fmt::Display for RefreshToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Where the one refresh token lives.
pub trait RefreshTokenStore {
    /// Creates or replaces the stored token.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Keychain`] when the backing store rejects the write.
    fn put(&self, token: &RefreshToken) -> Result<(), Error>;

    /// Returns the stored token, or `None` when nothing is stored.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Keychain`] when the backing store cannot be read and
    /// [`Error::KeychainEncoding`] when the stored bytes are not UTF-8.
    fn get(&self) -> Result<Option<RefreshToken>, Error>;

    /// Removes the stored token. Deleting an absent token succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Keychain`] when the backing store rejects the delete.
    fn delete(&self) -> Result<(), Error>;
}

/// Process-local store for tests and the G2 spike.
#[derive(Debug, Default)]
pub struct InMemoryStore {
    token: Mutex<Option<RefreshToken>>,
}

impl RefreshTokenStore for InMemoryStore {
    fn put(&self, token: &RefreshToken) -> Result<(), Error> {
        *self.token.lock().unwrap_or_else(PoisonError::into_inner) = Some(token.clone());
        Ok(())
    }

    fn get(&self) -> Result<Option<RefreshToken>, Error> {
        Ok(self
            .token
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }

    fn delete(&self) -> Result<(), Error> {
        *self.token.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }
}

/// The macOS Keychain store. Exercised only by the live test under
/// `APPLEJACK_LIVE_STEAM=1`; an ad-hoc signed development build may prompt.
#[derive(Debug, Clone)]
pub struct SecurityFrameworkStore {
    service: String,
    account: String,
}

impl Default for SecurityFrameworkStore {
    fn default() -> Self {
        Self {
            service: SERVICE.to_owned(),
            account: ACCOUNT.to_owned(),
        }
    }
}

impl SecurityFrameworkStore {
    /// A store on a different item, for tests that must not touch the real one.
    pub fn with_names(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }
}

impl RefreshTokenStore for SecurityFrameworkStore {
    fn put(&self, token: &RefreshToken) -> Result<(), Error> {
        set_generic_password(&self.service, &self.account, token.expose().as_bytes())
            .map_err(keychain_error)
    }

    fn get(&self) -> Result<Option<RefreshToken>, Error> {
        let options = PasswordOptions::new_generic_password(&self.service, &self.account);
        match generic_password(options) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(|token| Some(RefreshToken(token)))
                .map_err(|_| Error::KeychainEncoding),
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(error) => Err(keychain_error(error)),
        }
    }

    fn delete(&self) -> Result<(), Error> {
        match delete_generic_password(&self.service, &self.account) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(error) => Err(keychain_error(error)),
        }
    }
}

fn keychain_error(error: security_framework::base::Error) -> Error {
    Error::Keychain {
        code: error.code(),
        message: error
            .message()
            .unwrap_or_else(|| "unknown Security framework error".to_owned()),
    }
}
