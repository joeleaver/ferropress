//! # ferropress-secrets-env
//!
//! The baseline [`SecretStore`] adapter: deployment secrets come from the process
//! environment, optionally seeded from a `.env` file via `dotenvy`. This is the
//! zero-dependency-default for self-hosting — no vault, no host metadata service.
//!
//! INVARIANT (PORT shape): the [`SecretStore`] port is engine-shaped, so this
//! adapter takes NO `project_id` / namespace / host-token. A secret is addressed
//! purely by its [`SecretRef`] key, which maps 1:1 to an environment-variable
//! name (e.g. `SecretRef("SMTP_PASSWORD")` -> `$SMTP_PASSWORD`).
//!
//! A future vault/host adapter (e.g. jkbase-backed) is a *different* crate
//! implementing the same port; this one stays the portable default.

use async_trait::async_trait;

use ferropress_core::error::{CoreError, Result as CoreResult};
use ferropress_core::ports::{SecretRef, SecretStore};

/// Reads secrets from the process environment, optionally after loading a dotenv
/// file. Construction performs the (idempotent) dotenv load; lookups then read
/// `std::env` directly so later-`set`-in-process vars are also visible.
#[derive(Debug, Clone, Default)]
pub struct EnvSecretStore {
    // No state today: env is global. Kept as a unit-ish struct so we can later
    // hold an in-memory overlay / a prefix without changing the public API.
    _private: (),
}

impl EnvSecretStore {
    /// Build a store reading directly from the current process environment, with
    /// NO dotenv side effect. Use when the environment is already populated (e.g.
    /// systemd `EnvironmentFile`, container env).
    pub fn from_env() -> Self {
        Self { _private: () }
    }

    /// Build a store after loading the default `.env` from the current working
    /// directory (if present). A missing `.env` is NOT an error — env-only
    /// deployments are the common case.
    pub fn load() -> CoreResult<Self> {
        // A missing `.env` is the common case (pure-env deployments) and is NOT
        // an error; any *other* dotenv failure (malformed file, unreadable) maps
        // to `Unavailable`. dotenvy reports the missing-file case via
        // `Error::Io` with `ErrorKind::NotFound`.
        match dotenvy::dotenv() {
            Ok(_) => Ok(Self::from_env()),
            Err(e) if e.not_found() => Ok(Self::from_env()),
            Err(e) => Err(CoreError::Unavailable(format!("dotenv load failed: {e}"))),
        }
    }

    /// Build a store after loading a dotenv file from an explicit path.
    ///
    /// Unlike [`load`], a missing file here is a genuine failure: the caller
    /// named an explicit path, so its absence is a misconfiguration.
    pub fn load_from(path: impl AsRef<std::path::Path>) -> CoreResult<Self> {
        match dotenvy::from_path(path.as_ref()) {
            Ok(()) => Ok(Self::from_env()),
            Err(e) => Err(CoreError::Unavailable(format!(
                "dotenv load from {} failed: {e}",
                path.as_ref().display()
            ))),
        }
    }
}

#[async_trait]
impl SecretStore for EnvSecretStore {
    async fn get(&self, key: &SecretRef) -> CoreResult<String> {
        // `key.0` is the env-var *name* (safe to surface); the *value* is never
        // logged or placed in an error. Both `NotPresent` and `NotUnicode`
        // collapse to `Unavailable` here: a secret that cannot be read at all is,
        // for the caller, simply unavailable. (`try_get` is the API that splits
        // "absent" from "present-but-broken".)
        std::env::var(&key.0)
            .map_err(|_| CoreError::Unavailable(format!("secret {} not set", key.0)))
    }

    async fn try_get(&self, key: &SecretRef) -> CoreResult<Option<String>> {
        // Distinguish a plainly-absent var (Ok(None)) from a present-but-invalid
        // (non-UTF8) value, which is a genuine misconfiguration -> Validation.
        // Again the value itself is never included in any message.
        match std::env::var(&key.0) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(CoreError::Validation(format!(
                "secret {} holds a non-UTF8 value",
                key.0
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // We never mutate the process environment: `std::env::set_var` is `unsafe`
    // (Rust 2024) and the workspace forbids `unsafe`. So the "present" cases read
    // `PATH` (always set in a normal process), and the "absent" cases use a name
    // that cannot plausibly be set. We assert shape/presence, never a secret value.
    const PRESENT_VAR: &str = "PATH";
    const ABSENT_VAR: &str = "FERROPRESS_SECRETS_ENV_TEST_DEFINITELY_ABSENT_b7f3a1";

    #[tokio::test]
    async fn get_returns_present_value() {
        let store = EnvSecretStore::from_env();
        let value = store
            .get(&SecretRef(PRESENT_VAR.to_string()))
            .await
            .expect("a present env var should resolve");
        assert!(!value.is_empty(), "$PATH should be non-empty");
    }

    #[tokio::test]
    async fn get_missing_is_unavailable() {
        let store = EnvSecretStore::from_env();
        let err = store
            .get(&SecretRef(ABSENT_VAR.to_string()))
            .await
            .expect_err("absent secret should be an error");
        assert!(matches!(err, CoreError::Unavailable(_)), "got {err:?}");
        // The key *name* may appear in the message; there is no value to leak.
        assert!(err.to_string().contains(ABSENT_VAR));
    }

    #[tokio::test]
    async fn try_get_present_is_some() {
        let store = EnvSecretStore::from_env();
        let value = store
            .try_get(&SecretRef(PRESENT_VAR.to_string()))
            .await
            .expect("try_get should not error");
        assert!(value.is_some_and(|v| !v.is_empty()));
    }

    #[tokio::test]
    async fn try_get_missing_is_none() {
        let store = EnvSecretStore::from_env();
        let value = store
            .try_get(&SecretRef(ABSENT_VAR.to_string()))
            .await
            .expect("absent secret must be Ok(None), not an error");
        assert_eq!(value, None);
    }

    #[test]
    fn debug_does_not_leak_state() {
        // The Debug impl is derived over a unit-ish struct, so it can never
        // render a secret value. This pins that contract.
        let store = EnvSecretStore::from_env();
        let rendered = format!("{store:?}");
        assert_eq!(rendered, "EnvSecretStore { _private: () }");
    }
}
