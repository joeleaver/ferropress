//! HMAC-SHA256 stateless session tokens.
//!
//! A token is `base64url(payload_json) "." base64url(hmac_sha256(key, payload))`.
//! The payload ([`SessionClaims`]) carries the user id, role, and an expiry, so a
//! request is authenticated + authorized from the token alone — no session store,
//! no per-request user read. Verification recomputes the MAC in constant time,
//! then checks expiry. Rotating the signing key invalidates every outstanding
//! token (an effective global logout).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use ferropress_core::role::Role;

type HmacSha256 = Hmac<Sha256>;

/// The 32-byte HMAC signing key. Derive it from an operator secret (any length)
/// via [`SigningKey::derive_from_secret`], or supply raw bytes. Keep it secret;
/// anyone with it can mint valid tokens.
#[derive(Clone)]
pub struct SigningKey([u8; 32]);

impl SigningKey {
    /// Use raw 32 key bytes directly.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Derive a key by SHA-256 of an operator-supplied secret string (so a secret
    /// of any length/shape from the `SecretStore` yields a fixed 32-byte key).
    pub fn derive_from_secret(secret: &str) -> Self {
        Self(Sha256::digest(secret.as_bytes()).into())
    }
}

// Never print key material.
impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SigningKey(<redacted>)")
    }
}

/// The signed, self-describing session payload. `exp`/`iat` are epoch **millis**
/// (UTC), matching `ferropress_core::value::now_millis`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClaims {
    /// The authenticated user's object id.
    pub sub: u64,
    /// The user's role at login, so authorization needs no user re-read.
    pub role: Role,
    /// Issued-at (epoch millis).
    pub iat: i64,
    /// Expiry (epoch millis); the token is invalid at/after this instant.
    pub exp: i64,
}

impl SessionClaims {
    /// Build claims issued at `issued_ms` that expire `ttl_ms` later.
    pub fn new(user_id: u64, role: Role, issued_ms: i64, ttl_ms: i64) -> Self {
        Self {
            sub: user_id,
            role,
            iat: issued_ms,
            exp: issued_ms.saturating_add(ttl_ms),
        }
    }
}

/// Why a token failed verification. All variants are treated the same by callers
/// (reject → 401); the distinction exists for logging/tests.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenError {
    /// Not `payload.signature`, or a part wasn't valid base64url.
    #[error("malformed token")]
    Malformed,
    /// The signature did not match (forged/tampered/wrong key).
    #[error("bad token signature")]
    BadSignature,
    /// The payload decoded but wasn't valid claims JSON.
    #[error("invalid token payload")]
    Payload,
    /// `now >= exp`.
    #[error("token expired")]
    Expired,
}

/// Mint a signed token for `claims`.
pub fn mint(claims: &SessionClaims, key: &SigningKey) -> String {
    // `to_vec` on a plain struct of scalars/enum can't fail; encode defensively.
    let payload = serde_json::to_vec(claims).unwrap_or_default();
    let payload_b64 = URL_SAFE_NO_PAD.encode(&payload);

    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts a 32-byte key");
    mac.update(payload_b64.as_bytes());
    let sig = mac.finalize().into_bytes();
    let sig_b64 = URL_SAFE_NO_PAD.encode(sig);

    format!("{payload_b64}.{sig_b64}")
}

/// Verify a token against `key` and check it hasn't expired at `now_ms`. On
/// success returns the decoded claims; otherwise a [`TokenError`]. The MAC is
/// verified in constant time BEFORE the payload is trusted/parsed.
pub fn verify(token: &str, key: &SigningKey, now_ms: i64) -> Result<SessionClaims, TokenError> {
    let (payload_b64, sig_b64) = token.split_once('.').ok_or(TokenError::Malformed)?;

    let sig = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| TokenError::Malformed)?;

    // Constant-time MAC check over the exact base64 payload bytes.
    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts a 32-byte key");
    mac.update(payload_b64.as_bytes());
    mac.verify_slice(&sig)
        .map_err(|_| TokenError::BadSignature)?;

    // Signature is good — now the payload is trusted enough to decode.
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| TokenError::Malformed)?;
    let claims: SessionClaims =
        serde_json::from_slice(&payload).map_err(|_| TokenError::Payload)?;

    if now_ms >= claims.exp {
        return Err(TokenError::Expired);
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SigningKey {
        SigningKey::derive_from_secret("test-secret")
    }

    fn claims() -> SessionClaims {
        // issued at t=1000, valid for 1000ms (expires at 2000).
        SessionClaims::new(42, Role::Editor, 1_000, 1_000)
    }

    #[test]
    fn mint_then_verify_roundtrips() {
        let token = mint(&claims(), &key());
        let got = verify(&token, &key(), 1_500).expect("valid before expiry");
        assert_eq!(got, claims());
        assert_eq!(got.sub, 42);
        assert_eq!(got.role, Role::Editor);
    }

    #[test]
    fn expired_token_is_rejected() {
        let token = mint(&claims(), &key());
        assert_eq!(verify(&token, &key(), 2_000), Err(TokenError::Expired));
        assert_eq!(verify(&token, &key(), 9_999), Err(TokenError::Expired));
    }

    #[test]
    fn wrong_key_is_bad_signature() {
        let token = mint(&claims(), &key());
        let other = SigningKey::derive_from_secret("different-secret");
        assert_eq!(verify(&token, &other, 1_500), Err(TokenError::BadSignature));
    }

    #[test]
    fn tampered_payload_is_bad_signature() {
        let token = mint(&claims(), &key());
        let (_payload, sig) = token.split_once('.').unwrap();
        // Forge a payload claiming Administrator; keep the old signature.
        let forged_claims = SessionClaims::new(42, Role::Administrator, 1_000, 1_000);
        let forged_payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged_claims).unwrap());
        let forged = format!("{forged_payload}.{sig}");
        assert_eq!(
            verify(&forged, &key(), 1_500),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        let k = key();
        assert_eq!(verify("no-dot", &k, 1_500), Err(TokenError::Malformed));
        assert!(verify("a.b.c", &k, 1_500).is_err());
        assert_eq!(verify("!!!.???", &k, 1_500), Err(TokenError::Malformed));
    }

    #[test]
    fn signing_key_debug_is_redacted() {
        assert_eq!(format!("{:?}", key()), "SigningKey(<redacted>)");
    }
}
