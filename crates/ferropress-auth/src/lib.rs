//! # ferropress-auth
//!
//! Admin authentication primitives, kept deliberately small and transport-free so
//! they are pure and host-unit-testable:
//!
//! * [`password`] — Argon2id password hashing + constant-time verification against
//!   the `User.password_hash` column.
//! * [`token`] — HMAC-SHA256 **stateless** session tokens. Ferropress's schema is
//!   designed around signed tokens with no `Session` entity, so a login mints a
//!   self-describing, signed, expiring token; verification needs only the signing
//!   key (no store lookup). The token is carried in an HttpOnly cookie that
//!   `ferropress-http` sets/reads from raw headers (no cookie crate).
//!
//! This crate knows nothing about HTTP, cookies, rhypedb, or rinch. It depends on
//! `ferropress-core` only for [`Role`](ferropress_core::role::Role), which travels
//! inside the token so a request can be authorized without re-reading the user.

pub mod password;
pub mod token;

pub use password::{hash_password, verify_dummy, verify_password};
pub use token::{SessionClaims, SigningKey, TokenError};
