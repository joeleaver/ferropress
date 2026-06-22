//! `User` — account + role. Credentials are an Argon2 hash (never plaintext);
//! deployment secrets (SMTP creds, etc.) live behind `SecretStore`, NOT here.
//! Profile fields are typed columns rather than usermeta. The dead WP
//! `user_status` int is dropped; an activation token is kept, plus the
//! password-reset token/expiry pair the auth flow needs (single-site, stateless
//! signed tokens — there is no Session entity, so the reset token lives here).

use time::OffsetDateTime;

use crate::role::Role;
use crate::value::ObjectId;

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub id: Option<ObjectId>,
    pub uuid: String,
    /// Login / username (WP `user_login` + `user_nicename`); unique slug.
    pub slug: String,
    pub display_name: String,
    /// Unique.
    pub email: String,
    pub role: Role,
    /// Argon2 password hash (string-encoded). Empty for SSO-only accounts.
    pub password_hash: String,
    pub url: Option<String>,
    pub bio: String,
    /// display_name + bio; optional `@vectorize` source for author search.
    pub plaintext: String,
    /// Activation token for new accounts (None once activated).
    pub activation_key: Option<String>,
    /// Single-use password-reset token (None when no reset is pending). Stored
    /// `@indexed` in the SDL so the reset flow can look a user up by it.
    pub password_reset_token: Option<String>,
    /// Expiry instant for `password_reset_token` (stored as a native `DateTime`).
    pub password_reset_expires: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub meta: serde_json::Value,

    pub avatar_media: Option<ObjectId>, // -> Media
}
