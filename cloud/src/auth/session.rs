//! Browser sessions: the `__Host-mc_session` cookie maps (by SHA-256) to a
//! session row. Mutations additionally need the session's CSRF token in
//! `X-CSRF-Token` and an `Origin` equal to the public origin.

use axum::extract::{FromRef, FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::{constant_time_eq, cookie_header, read_cookie, sha256};
use crate::error::AppError;
use crate::protocol::{Me, COOKIE_SESSION, HEADER_CSRF_TOKEN};
use crate::state::AppState;

/// An authenticated account. For non-safe methods (anything but GET/HEAD)
/// extraction also enforces CSRF token and Origin.
pub struct Session {
    pub user_id: uuid::Uuid,
    pub email: String,
    pub csrf_token: String,
    pub token_hash: Vec<u8>,
}

impl<S> FromRequestParts<S> for Session
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AppError> {
        let state = AppState::from_ref(state);
        let token =
            read_cookie(&parts.headers, COOKIE_SESSION).ok_or_else(AppError::unauthenticated)?;
        let token_hash = sha256(token.as_bytes());
        let row: Option<(uuid::Uuid, String, String)> = sqlx::query_as(
            "SELECT s.user_id, u.email, s.csrf_token FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.expires_at > now()",
        )
        .bind(&token_hash)
        .fetch_optional(&state.db)
        .await?;
        let (user_id, email, csrf_token) = row.ok_or_else(AppError::unauthenticated)?;
        let session = Session {
            user_id,
            email,
            csrf_token,
            token_hash,
        };
        if !matches!(parts.method, Method::GET | Method::HEAD) {
            session.check_mutation(parts, &state)?;
        }
        Ok(session)
    }
}

impl Session {
    fn check_mutation(&self, parts: &Parts, state: &AppState) -> Result<(), AppError> {
        let expected = state.config.public_url.origin().ascii_serialization();
        let origin = parts
            .headers
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        if origin != Some(expected.as_str()) {
            return Err(AppError::csrf_failed("missing or foreign Origin"));
        }
        let sent = parts
            .headers
            .get(HEADER_CSRF_TOKEN)
            .map(|v| v.as_bytes())
            .unwrap_or_default();
        if !constant_time_eq(sent, self.csrf_token.as_bytes()) {
            return Err(AppError::csrf_failed("missing or wrong X-CSRF-Token"));
        }
        Ok(())
    }
}

/// `GET /v1/me`.
pub async fn me(session: Session) -> Json<Me> {
    Json(Me {
        id: session.user_id.to_string(),
        email: session.email,
        csrf_token: session.csrf_token,
    })
}

/// `POST /v1/auth/logout`: with the session cookie, invalidates the session
/// and clears the cookie; with `Authorization: Bearer mca_...` (CLI logout),
/// revokes that account token. Both answer `204`.
pub async fn logout(
    State(state): State<AppState>,
    account: super::account::Account,
) -> Result<Response, AppError> {
    if !account.is_session {
        sqlx::query("DELETE FROM account_tokens WHERE token_hash = $1")
            .bind(&account.token_hash)
            .execute(&state.db)
            .await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(&account.token_hash)
        .execute(&state.db)
        .await?;

    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie_header(COOKIE_SESSION, "", 0));
    Ok(response)
}
