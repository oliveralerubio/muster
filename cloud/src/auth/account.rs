//! The account behind a request: a browser session (cookie + CSRF/Origin on
//! mutations) or an account token from the CLI device flow
//! (`Authorization: Bearer mca_...`) limited to its explicit scopes.
//!
//! Neither one is a machine: device tokens (`mcd_...`) are refused here.

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum::http::StatusCode;

use super::session::Session;
use super::sha256;
use crate::error::AppError;
use crate::protocol::{error_codes, ACCOUNT_TOKEN_PREFIX};
use crate::state::AppState;

pub struct Account {
    pub user_id: uuid::Uuid,
    /// `None` for a browser session (full console access); the token's
    /// scopes otherwise.
    scopes: Option<Vec<String>>,
    pub token_hash: Vec<u8>,
    pub is_session: bool,
}

impl Account {
    /// Refuses a token that lacks `scope` with `403 insufficient_scope`.
    pub fn require_scope(&self, scope: &str) -> Result<(), AppError> {
        match &self.scopes {
            None => Ok(()),
            Some(granted) if granted.iter().any(|s| s == scope) => Ok(()),
            Some(_) => Err(AppError::new(
                StatusCode::FORBIDDEN,
                error_codes::INSUFFICIENT_SCOPE,
                "this token does not allow the operation",
            )
            .with_details(serde_json::json!({ "required_scope": scope }))),
        }
    }
}

/// Value of `Authorization: Bearer <token>`, if any.
pub fn bearer(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
}

impl<S> FromRequestParts<S> for Account
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AppError> {
        let Some(token) = bearer(parts) else {
            let session = Session::from_request_parts(parts, state).await?;
            return Ok(Account {
                user_id: session.user_id,
                scopes: None,
                token_hash: session.token_hash,
                is_session: true,
            });
        };
        if !token.starts_with(ACCOUNT_TOKEN_PREFIX) {
            return Err(AppError::unauthenticated());
        }
        let state = AppState::from_ref(state);
        let row: Option<(uuid::Uuid, Vec<String>)> = sqlx::query_as(
            "SELECT user_id, scopes FROM account_tokens WHERE token_hash = $1 AND expires_at > now()",
        )
        .bind(sha256(token.as_bytes()))
        .fetch_optional(&state.db)
        .await?;
        let (user_id, scopes) = row.ok_or_else(AppError::unauthenticated)?;
        Ok(Account {
            user_id,
            scopes: Some(scopes),
            token_hash: sha256(token.as_bytes()),
            is_session: false,
        })
    }
}
