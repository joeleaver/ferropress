//! Session lifecycle: `login` (verify password → mint token → set cookie),
//! `logout` (clear cookie), `me` (who the current session belongs to).

use axum::Json;
use axum::extract::State;
use axum::http::header::SET_COOKIE;
use axum::response::{AppendHeaders, IntoResponse};
use serde::{Deserialize, Serialize};

use ferropress_auth::{SessionClaims, token, verify_dummy, verify_password};
use ferropress_core::USER_TYPE;
use ferropress_core::error::CoreError;
use ferropress_core::query::{Compare, FilterSpec};
use ferropress_core::role::Role;
use ferropress_core::value::{TypeName, Value, now_millis};

use super::{AdminError, AdminJson, AuthedUser, str_field};
use crate::AppState;

/// Credentials POSTed to `/admin/api/login`.
#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// The safe public shape of the signed-in user (never the password hash).
#[derive(Serialize)]
pub struct UserDto {
    pub id: u64,
    pub username: String,
    pub display_name: String,
    pub role: Role,
}

#[derive(Serialize)]
pub struct SessionResponse {
    pub user: UserDto,
}

/// `POST /admin/api/login` — verify credentials, mint a session token, set the
/// HttpOnly cookie. On any failure (unknown user OR bad password) returns a single
/// uniform 401 so the response can't be used to probe which usernames exist.
pub async fn login(
    State(state): State<AppState>,
    AdminJson(body): AdminJson<LoginRequest>,
) -> Result<impl IntoResponse, AdminError> {
    let admin = state.admin.as_ref().ok_or(AdminError::Unauthorized)?;

    // Look the user up by login slug (unique, indexed).
    let found = state
        .store
        .filter(FilterSpec {
            type_name: TypeName::from(USER_TYPE),
            field: "slug".to_owned(),
            op: Compare::Eq,
            value: Value::String(body.username.clone()),
            limit: Some(1),
        })
        .await?
        .into_iter()
        .next();

    // Uniform 401 for BOTH an unknown user and a wrong password — same body AND
    // (via `verify_dummy`) same wall-clock: an absent user still pays an Argon2
    // verify, so response latency can't be used to enumerate valid usernames.
    let Some(user) = found else {
        verify_dummy(&body.password);
        return Err(AdminError::Unauthorized);
    };

    let hash = str_field(&user, "password_hash").unwrap_or_default();
    if !verify_password(&hash, &body.password) {
        return Err(AdminError::Unauthorized);
    }

    let role = role_of(&user)?;
    let now = now_millis();
    let claims = SessionClaims::new(user.id.0, role, now, admin.session_ttl_ms);
    let token = token::mint(&claims, &admin.signing_key);
    let cookie = admin.cookie(&token, false);

    let dto = user_dto(&user, role);
    Ok((
        AppendHeaders([(SET_COOKIE, cookie)]),
        Json(SessionResponse { user: dto }),
    ))
}

/// `POST /admin/api/logout` — clear the session cookie. Idempotent; needs no valid
/// session (signing out an already-invalid session is a no-op success).
pub async fn logout(State(state): State<AppState>) -> Result<impl IntoResponse, AdminError> {
    let admin = state.admin.as_ref().ok_or(AdminError::Unauthorized)?;
    let cookie = admin.cookie("", true);
    Ok(AppendHeaders([(SET_COOKIE, cookie)]))
}

/// `GET /admin/api/me` — the current session's user. A session for a since-deleted
/// user is treated as unauthenticated (401).
pub async fn me(
    State(state): State<AppState>,
    who: AuthedUser,
) -> Result<Json<SessionResponse>, AdminError> {
    let user = match state.store.get(&TypeName::from(USER_TYPE), who.id).await {
        Ok(user) => user,
        // A since-deleted user's still-valid token is treated as signed-out (401)...
        Err(CoreError::NotFound { .. }) => return Err(AdminError::Unauthorized),
        // ...but a real backend fault is a 500 (logged), NOT a spurious logout.
        Err(other) => return Err(AdminError::Internal(other)),
    };
    let role = role_of(&user)?;
    Ok(Json(SessionResponse {
        user: user_dto(&user, role),
    }))
}

/// Parse the stored snake_case `role` string into a [`Role`]. A missing/unparseable
/// role is a data-integrity fault (500), not a client error.
fn role_of(user: &ferropress_core::value::Object) -> Result<Role, AdminError> {
    let raw = str_field(user, "role").unwrap_or_default();
    serde_json::from_value::<Role>(serde_json::Value::String(raw.clone())).map_err(|_| {
        AdminError::Internal(CoreError::Store(format!(
            "user {} has an unparseable role {raw:?}",
            user.id.0
        )))
    })
}

fn user_dto(user: &ferropress_core::value::Object, role: Role) -> UserDto {
    UserDto {
        id: user.id.0,
        username: str_field(user, "slug").unwrap_or_default(),
        display_name: str_field(user, "display_name").unwrap_or_default(),
        role,
    }
}
