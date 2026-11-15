//! OAuth 2.0 Device Authorization Grant (RFC 8628) for CLI login.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use rand::Rng;
use time::OffsetDateTime;

use super::session::Session;
use super::{random_token, sha256};
use crate::error::AppError;
use crate::protocol::{
    error_codes, scopes, AccountToken, DeviceApproval, DeviceApproveRequest, DeviceAuthorization,
    DeviceAuthorizeRequest, DeviceDecision, DeviceTokenRequest, ACCOUNT_TOKEN_PREFIX,
    DEVICE_CODE_GRANT_TYPE,
};
use crate::state::AppState;

const USER_CODE_CHARS: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ23456789";
const CODE_TTL_SECS: i64 = 600;
const INITIAL_INTERVAL_SECS: i32 = 5;
const ACCOUNT_TOKEN_TTL_DAYS: i64 = 30;

fn generate_user_code() -> String {
    let mut rng = rand::thread_rng();
    let part1: String = (0..4)
        .map(|_| {
            let idx = rng.gen_range(0..USER_CODE_CHARS.len());
            USER_CODE_CHARS[idx] as char
        })
        .collect();
    let part2: String = (0..4)
        .map(|_| {
            let idx = rng.gen_range(0..USER_CODE_CHARS.len());
            USER_CODE_CHARS[idx] as char
        })
        .collect();
    format!("{part1}-{part2}")
}

fn normalize_user_code(raw: &str) -> String {
    raw.trim().replace('-', "").to_ascii_uppercase()
}

/// `POST /v1/device/authorize`: starts the device login flow.
pub async fn authorize(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Json<DeviceAuthorization>, AppError> {
    let req: DeviceAuthorizeRequest = crate::tenancy::parse_json(&body)?;
    let client_name = req.client_name.trim();
    if client_name.is_empty() || client_name.len() > 64 || client_name.chars().any(char::is_control)
    {
        return Err(AppError::bad_request(
            "client_name must be 1-64 characters without control characters",
        ));
    }

    let device_code = format!("dev_{}", random_token());
    let device_code_hash = sha256(device_code.as_bytes());
    let user_code = generate_user_code();
    let normalized_code = normalize_user_code(&user_code);

    let expires_at = OffsetDateTime::now_utc() + time::Duration::seconds(CODE_TTL_SECS);

    sqlx::query(
        "INSERT INTO device_authorizations (device_code_hash, user_code, client_name, status, interval_secs, expires_at)
         VALUES ($1, $2, $3, 'pending', $4, $5)",
    )
    .bind(&device_code_hash)
    .bind(&normalized_code)
    .bind(client_name)
    .bind(INITIAL_INTERVAL_SECS)
    .bind(expires_at)
    .execute(&state.db)
    .await?;

    let verification_uri = format!(
        "{}/device",
        state.config.public_url.as_str().trim_end_matches('/')
    );
    let verification_uri_complete = format!("{verification_uri}?user_code={user_code}");

    Ok(Json(DeviceAuthorization {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete,
        expires_in: CODE_TTL_SECS as u64,
        interval: INITIAL_INTERVAL_SECS as u64,
    }))
}

/// `POST /v1/device/approve`: signed-in user approves or denies the CLI login.
pub async fn approve(
    State(state): State<AppState>,
    session: Session,
    body: axum::body::Bytes,
) -> Result<Json<DeviceApproval>, AppError> {
    let req: DeviceApproveRequest = crate::tenancy::parse_json(&body)?;
    let normalized_code = normalize_user_code(&req.user_code);
    let mut tx = state.db.begin().await?;

    let row: Option<(String, String, OffsetDateTime)> = sqlx::query_as(
        "SELECT client_name, status, expires_at FROM device_authorizations
         WHERE user_code = $1 FOR UPDATE",
    )
    .bind(&normalized_code)
    .fetch_optional(&mut *tx)
    .await?;

    let (client_name, status, expires_at) =
        row.ok_or_else(|| AppError::bad_request("invalid or unknown user code"))?;

    if OffsetDateTime::now_utc() > expires_at {
        return Err(AppError::bad_request("user code has expired"));
    }

    if status != "pending" {
        return Err(AppError::bad_request("user code was already used"));
    }

    let new_status = match req.decision {
        DeviceDecision::Approve => "approved",
        DeviceDecision::Deny => "denied",
    };

    sqlx::query("UPDATE device_authorizations SET status = $1, user_id = $2 WHERE user_code = $3")
        .bind(new_status)
        .bind(session.user_id)
        .bind(&normalized_code)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(Json(DeviceApproval {
        status: new_status.to_string(),
        client_name,
    }))
}

/// `POST /v1/device/token`: CLI polls for the access token.
pub async fn token(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Json<AccountToken>, AppError> {
    let req: DeviceTokenRequest = crate::tenancy::parse_json(&body)?;
    if req.grant_type != DEVICE_CODE_GRANT_TYPE {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            error_codes::INVALID_GRANT,
            "unsupported grant_type",
        ));
    }

    let device_code_hash = sha256(req.device_code.as_bytes());
    let mut tx = state.db.begin().await?;

    type AuthorizationRow = (
        Option<uuid::Uuid>,
        String,
        String,
        i32,
        Option<OffsetDateTime>,
        OffsetDateTime,
    );
    let row: Option<AuthorizationRow> = sqlx::query_as(
        "SELECT user_id, client_name, status, interval_secs, last_polled_at, expires_at
         FROM device_authorizations WHERE device_code_hash = $1 FOR UPDATE",
    )
    .bind(&device_code_hash)
    .fetch_optional(&mut *tx)
    .await?;

    let (user_id, client_name, status, interval_secs, last_polled_at, expires_at) = row
        .ok_or_else(|| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                error_codes::INVALID_GRANT,
                "invalid device code",
            )
        })?;

    let now = OffsetDateTime::now_utc();
    if now > expires_at {
        sqlx::query("DELETE FROM device_authorizations WHERE device_code_hash = $1")
            .bind(&device_code_hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            error_codes::EXPIRED_TOKEN,
            "device code expired",
        ));
    }

    // Rate limiting: poll interval enforcement applies while pending
    if status == "pending" {
        if let Some(last_poll) = last_polled_at {
            let diff = now - last_poll;
            if diff.whole_seconds() < interval_secs as i64 {
                let new_interval = interval_secs + 5;
                sqlx::query(
                    "UPDATE device_authorizations SET interval_secs = $1, last_polled_at = $2
                     WHERE device_code_hash = $3",
                )
                .bind(new_interval)
                .bind(now)
                .bind(&device_code_hash)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;

                return Err(AppError::new(
                    StatusCode::BAD_REQUEST,
                    error_codes::SLOW_DOWN,
                    "polled too fast, slow down",
                )
                .with_details(serde_json::json!({ "interval": new_interval })));
            }
        }
    }

    sqlx::query("UPDATE device_authorizations SET last_polled_at = $1 WHERE device_code_hash = $2")
        .bind(now)
        .bind(&device_code_hash)
        .execute(&mut *tx)
        .await?;

    match status.as_str() {
        "pending" => {
            tx.commit().await?;
            Err(AppError::new(
                StatusCode::BAD_REQUEST,
                error_codes::AUTHORIZATION_PENDING,
                "authorization pending",
            ))
        }
        "denied" => {
            sqlx::query("DELETE FROM device_authorizations WHERE device_code_hash = $1")
                .bind(&device_code_hash)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Err(AppError::new(
                StatusCode::BAD_REQUEST,
                error_codes::ACCESS_DENIED,
                "authorization denied by user",
            ))
        }
        "approved" => {
            let user_id = user_id.ok_or_else(|| AppError::internal("approved without user_id"))?;
            sqlx::query("DELETE FROM device_authorizations WHERE device_code_hash = $1")
                .bind(&device_code_hash)
                .execute(&mut *tx)
                .await?;

            let access_token = format!("{ACCOUNT_TOKEN_PREFIX}{}", random_token());
            let token_hash = sha256(access_token.as_bytes());
            let expires_at = now + time::Duration::days(ACCOUNT_TOKEN_TTL_DAYS);
            let scope_list = vec![
                scopes::WORKSPACES_READ.to_string(),
                scopes::DEVICES_LINK.to_string(),
            ];

            sqlx::query(
                "INSERT INTO account_tokens (token_hash, user_id, client_name, scopes, expires_at)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(&token_hash)
            .bind(user_id)
            .bind(&client_name)
            .bind(&scope_list)
            .bind(expires_at)
            .execute(&mut *tx)
            .await?;

            tx.commit().await?;

            Ok(Json(AccountToken {
                access_token,
                token_type: "Bearer".into(),
                expires_in: (ACCOUNT_TOKEN_TTL_DAYS * 86400) as u64,
                scope: scope_list,
                account_id: user_id.to_string(),
            }))
        }
        _ => Err(AppError::internal("invalid device authorization status")),
    }
}
