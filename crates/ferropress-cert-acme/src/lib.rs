//! # ferropress-cert-acme
//!
//! The baseline [`CertSource`] adapter, in two modes:
//!   * **No-op / proxy mode** ([`CertSource`] returns `None`): Ferropress sits
//!     behind a TLS-terminating reverse proxy (nginx/Caddy/host LB) and serves
//!     plain HTTP. This is the recommended self-host default and needs no certs
//!     in-process.
//!   * **Embedded ACME mode**: `rustls-acme` obtains/renews certs via the
//!     ALPN-01 / HTTP-01 challenge for a domain.
//!
//! INVARIANT (PORT shape): the port is engine-shaped. There is deliberately NO
//! DNS-01 challenge surface and NO host/provider coupling (no jkbase, no cloud
//! DNS API) — those would leak a specific host's shape into the abstraction.
//! Only the two portable challenge types live here.

use async_trait::async_trait;

use ferropress_core::error::Result as CoreResult;
use ferropress_core::ports::{CertSource, Certificate};

/// TLS certificate source. Pick the mode at composition time in
/// `ferropress-server` (the only place adapters are selected).
pub enum AcmeCertSource {
    /// Behind a TLS-terminating proxy: never issues a cert, always resolves to
    /// `None` so the HTTP server serves plain HTTP.
    Proxy,
    /// Embedded ACME via `rustls-acme`.
    Acme(AcmeConfig),
}

/// Configuration for embedded ACME issuance. Intentionally minimal and
/// host-agnostic: domains + contact + a cache directory + the directory URL
/// (Let's Encrypt prod/staging). NO DNS provider credentials (DNS-01 is out of
/// scope by design).
#[derive(Debug, Clone)]
pub struct AcmeConfig {
    /// Domains to obtain certificates for.
    pub domains: Vec<String>,
    /// ACME account contact (e.g. `"mailto:ops@example.com"`).
    pub contact: Vec<String>,
    /// On-disk cache for the account key + issued certs (so restarts don't
    /// re-issue and hit rate limits).
    pub cache_dir: std::path::PathBuf,
    /// ACME directory URL. `None` = Let's Encrypt production default.
    pub directory_url: Option<String>,
}

impl AcmeCertSource {
    /// No-op proxy mode: the server terminates TLS upstream.
    pub fn proxy() -> Self {
        AcmeCertSource::Proxy
    }

    /// Embedded ACME mode from an explicit config.
    pub fn acme(config: AcmeConfig) -> Self {
        AcmeCertSource::Acme(config)
    }
}

#[async_trait]
impl CertSource for AcmeCertSource {
    async fn certificate(&self, _domain: &str) -> CoreResult<Option<Certificate>> {
        match self {
            // Proxy mode: no in-process cert. The server then binds plain HTTP.
            AcmeCertSource::Proxy => {
                // TODO (trivial): Ok(None). Left as todo!() only to keep the whole
                // adapter consistently "unimplemented" until the boot path needs it.
                todo!("proxy mode resolves to Ok(None)")
            }
            AcmeCertSource::Acme(_config) => {
                // TODO: drive rustls-acme for `domain` (must be in config.domains):
                // build an AcmeConfig/AcmeState with the cache dir + directory URL,
                // pump the cert order, and return the issued cert+key as PEM in a
                // `Certificate`. Map acme errors -> CoreError::Unavailable. The
                // long-lived renewal task is owned by the HTTP server's TLS
                // acceptor; this method resolves the *current* cert.
                todo!("obtain/renew via rustls-acme for the requested domain")
            }
        }
    }
}
