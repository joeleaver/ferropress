//! Argon2id password hashing + verification.
//!
//! Hashes are the standard PHC string form (`$argon2id$v=19$m=…,t=…,p=…$salt$hash`)
//! stored verbatim in `User.password_hash`. Verification is constant-time and
//! **fail-closed**: any parse/format/mismatch error (including an empty hash, i.e.
//! an SSO-only account with no password) returns `false`, never an error the caller
//! might mistake for success.

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

/// An error hashing a password. Verification never returns an error (it is
/// fail-closed to `false`); only hashing — a rare, non-attacker-controlled path
/// (account creation / password change) — can fail, and only on a crypto backend
/// fault.
#[derive(Debug, thiserror::Error)]
#[error("password hashing failed: {0}")]
pub struct HashError(String);

/// Hash a plaintext password with Argon2id (a fresh random salt, library-default
/// parameters). Returns the PHC string to store in `User.password_hash`. Used by
/// account creation / password change — NOT on the login hot path.
pub fn hash_password(password: &str) -> Result<String, HashError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| HashError(e.to_string()))
}

/// Verify a plaintext `password` against a stored Argon2 PHC `hash`, in constant
/// time. Returns `false` for any mismatch AND for any malformed/empty stored hash
/// (fail-closed) — an empty hash marks an SSO-only account that cannot password-log-in.
pub fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Run a full Argon2 verification against a fixed dummy hash and discard the
/// result. Call this on the **user-not-found** login path so a missing username
/// costs the same wall-clock as a real password check — closing the timing oracle
/// that would otherwise let an attacker enumerate valid usernames (a real user
/// runs Argon2; an absent one would return instantly). The dummy hash is computed
/// once and cached.
pub fn verify_dummy(password: &str) {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    // A valid PHC hash so `verify_password` actually executes Argon2 (an empty /
    // malformed hash would short-circuit and defeat the equalization).
    let hash = DUMMY.get_or_init(|| {
        hash_password("ferropress::timing-equalization::dummy-target").unwrap_or_default()
    });
    let _ = verify_password(hash, password);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_roundtrips() {
        let hash = hash_password("correct-horse-battery-staple").expect("hash");
        assert!(verify_password(&hash, "correct-horse-battery-staple"));
        assert!(!verify_password(&hash, "Correct-Horse-Battery-Staple"));
        assert!(!verify_password(&hash, ""));
    }

    #[test]
    fn distinct_salts_produce_distinct_hashes() {
        let a = hash_password("same").expect("hash a");
        let b = hash_password("same").expect("hash b");
        assert_ne!(a, b, "each hash must use a fresh salt");
        assert!(verify_password(&a, "same") && verify_password(&b, "same"));
    }

    #[test]
    fn verify_dummy_runs_without_panicking() {
        // It performs a real Argon2 verify against a cached dummy hash and discards
        // the result (used to equalize login timing on the user-not-found path).
        verify_dummy("whatever the attacker typed");
        verify_dummy(""); // even an empty candidate must be safe
    }

    #[test]
    fn empty_or_garbage_hash_is_fail_closed() {
        assert!(
            !verify_password("", "anything"),
            "empty hash never verifies"
        );
        assert!(!verify_password("not-a-phc-string", "anything"));
        assert!(!verify_password("$argon2id$broken", "anything"));
    }
}
