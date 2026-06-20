//! # ferropress-cert-acme
//!
//! The baseline [`CertSource`] adapter, in two modes:
//!   * **No-op / proxy mode** ([`AcmeCertSource::Proxy`]): Ferropress sits behind
//!     a TLS-terminating reverse proxy (nginx/Caddy/host LB) and serves plain
//!     HTTP. This is the recommended self-host default and needs no certs
//!     in-process. [`CertSource::certificate`] resolves to `Ok(None)`, so the
//!     HTTP server binds plain HTTP and lets the proxy terminate TLS. This mode
//!     is **fully implemented** and is the shipped baseline.
//!   * **Embedded ACME mode** ([`AcmeCertSource::Acme`]): drive `rustls-acme` to
//!     obtain/renew certs via the ALPN-01 / HTTP-01 challenge for a domain. This
//!     mode is **not yet wired**: the enum variant, constructor, and
//!     host-agnostic [`AcmeConfig`] exist so the composition surface in
//!     `ferropress-server` is stable and the future wiring point is named, but
//!     the `rustls-acme` order/renewal flow is out of baseline scope. Until it is
//!     built, `certificate` for this variant reports the port as
//!     [`CoreError::Unavailable`] rather than panicking — a real, honest result
//!     that keeps the trait method `todo!()`-free.
//!
//! INVARIANT (PORT shape): the port is engine-shaped. There is deliberately NO
//! DNS-01 challenge surface and NO host/provider coupling (no jkbase, no cloud
//! DNS API) — those would leak a specific host's shape into the abstraction.
//! Only the two portable challenge types live here.

use async_trait::async_trait;

use ferropress_core::error::CoreError;
use ferropress_core::error::Result as CoreResult;
use ferropress_core::ports::{CertSource, Certificate};

// The embedded-ACME path will drive `rustls-acme`'s own state machine. Its types
// (`AcmeConfig`/`AcmeState`/`AcmeAcceptor`) collide by name with this crate's
// host-agnostic `AcmeConfig`, so when the flow is wired it must be imported
// qualified, e.g. `use rustls_acme::AcmeConfig as RustlsAcmeConfig;`. We do not
// import it yet (it is unused until the ACME flow is built).

/// TLS certificate source. Pick the mode at composition time in
/// `ferropress-server` (the only place adapters are selected).
pub enum AcmeCertSource {
    /// Behind a TLS-terminating proxy: never issues a cert, always resolves to
    /// `None` so the HTTP server serves plain HTTP. This is the baseline.
    Proxy,
    /// Embedded ACME via `rustls-acme`. Constructed but not yet wired — see the
    /// module docs; `certificate` currently reports `Unavailable` for this mode.
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
    /// No-op proxy mode: the server terminates TLS upstream. Baseline.
    pub fn proxy() -> Self {
        AcmeCertSource::Proxy
    }

    /// Embedded ACME mode from an explicit config.
    ///
    /// NOTE: the `rustls-acme` issuance/renewal flow is not yet wired (out of
    /// baseline scope). Constructing this is valid, but [`CertSource::certificate`]
    /// will report [`CoreError::Unavailable`] for this variant until the flow is
    /// implemented. See the module docs.
    pub fn acme(config: AcmeConfig) -> Self {
        AcmeCertSource::Acme(config)
    }
}

#[async_trait]
impl CertSource for AcmeCertSource {
    async fn certificate(&self, domain: &str) -> CoreResult<Option<Certificate>> {
        match self {
            // Proxy mode (baseline): no in-process cert. The server then binds
            // plain HTTP and the upstream proxy terminates TLS.
            AcmeCertSource::Proxy => Ok(None),

            // Embedded ACME mode: not yet wired.
            //
            // TODO(rustls-acme): drive `rustls-acme` for `domain` (which must be
            // in `config.domains`): build an `AcmeState` from `cache_dir` +
            // `directory_url` + `contact`, pump the cert order to completion, and
            // return the issued chain + key as PEM in a `Certificate`. Map any
            // acme error -> `CoreError::Unavailable`. The long-lived renewal task
            // is owned by the HTTP server's TLS acceptor; this method resolves the
            // *current* cert. Until that is built we return `Unavailable` (a real,
            // non-panicking result) so the trait method stays `todo!()`-free.
            AcmeCertSource::Acme(config) => {
                debug_assert!(
                    config.domains.iter().any(|d| d == domain) || config.domains.is_empty(),
                    "requested domain should be one of the configured ACME domains",
                );
                Err(CoreError::Unavailable(format!(
                    "embedded ACME (rustls-acme) is not yet wired; \
                     cannot issue a certificate for {domain:?}"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn proxy_resolves_to_no_certificate() {
        let source = AcmeCertSource::proxy();
        let result = source.certificate("example.com").await;
        assert!(matches!(result, Ok(None)));
    }

    #[tokio::test]
    async fn proxy_returns_none_for_any_domain() {
        let source = AcmeCertSource::proxy();
        for domain in ["", "localhost", "sub.example.org", "xn--bcher-kva.example"] {
            let result = source.certificate(domain).await;
            assert!(
                matches!(result, Ok(None)),
                "proxy mode must report no certificate for domain {domain:?}",
            );
        }
    }

    #[tokio::test]
    async fn acme_variant_reports_unavailable_until_wired() {
        // The embedded-ACME path is a documented, not-yet-wired placeholder. It
        // must not panic (no todo!()) and must surface a real `Unavailable`.
        let source = AcmeCertSource::acme(AcmeConfig {
            domains: vec!["example.com".to_string()],
            contact: vec!["mailto:ops@example.com".to_string()],
            cache_dir: std::path::PathBuf::from("/tmp/ferropress-acme-cache"),
            directory_url: None,
        });
        let result = source.certificate("example.com").await;
        assert!(
            matches!(result, Err(CoreError::Unavailable(_))),
            "unwired ACME mode should report Unavailable, got {result:?}",
        );
    }
}
