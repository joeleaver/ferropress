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

use ferropress_core::error::Result as CoreResult;
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
        // TODO: dotenvy::dotenv() returns Err(NotFound) when there is no .env;
        // treat that as Ok(()), map any *other* dotenv error to
        // CoreError::Unavailable. Then return Self::from_env().
        todo!("load .env via dotenvy (NotFound is ok); return Self::from_env()")
    }

    /// Build a store after loading a dotenv file from an explicit path.
    pub fn load_from(_path: impl AsRef<std::path::Path>) -> CoreResult<Self> {
        // TODO: dotenvy::from_path(path); map a genuine load failure to
        // CoreError::Unavailable; return Self::from_env().
        todo!("load dotenv from explicit path; return Self::from_env()")
    }
}

#[async_trait]
impl SecretStore for EnvSecretStore {
    async fn get(&self, _key: &SecretRef) -> CoreResult<String> {
        // TODO: std::env::var(&key.0) -> Ok(val); map VarError to
        // CoreError::Unavailable(format!("secret {key} not set")).
        todo!("read env var by SecretRef; map absence to CoreError::Unavailable")
    }

    async fn try_get(&self, _key: &SecretRef) -> CoreResult<Option<String>> {
        // TODO: std::env::var(&key.0).ok() -> Ok(opt). A non-UTF8 value is the
        // only genuine error path (map to CoreError::Validation).
        todo!("read optional env var by SecretRef; absent -> Ok(None)")
    }
}
