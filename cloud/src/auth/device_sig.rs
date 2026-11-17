//! Device authentication and request signature verification for machine transport.

use axum::http::{HeaderMap, Method, StatusCode};
use time::OffsetDateTime;

use crate::auth::sha256;
use crate::error::AppError;
use crate::protocol::{
    body_sha256_hex, canonical_path_and_query, canonical_request_v1, error_codes, DeviceStatus,
    SignedRequestV1, DEVICE_TOKEN_PREFIX, HEADER_BASE_REVISION, HEADER_CONTENT_SHA256,
    HEADER_DEVICE_ID, HEADER_IDEMPOTENCY_KEY, HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP,
    SIGNED_REQUEST_MAX_SKEW_SECS,
};
use crate::state::AppState;

pub struct AuthenticatedDevice {
    pub id: uuid::Uuid,
    pub workspace_id: uuid::Uuid,
    pub machine_name: String,
    pub signing_key: String,
    pub recipient: String,
    pub status: DeviceStatus,
    pub bootstrap: bool,
}

/// Verifies a device-signed request. Returns the authenticated device.
pub async fn authenticate_device_request(
    state: &AppState,
    method: &Method,
    path: &str,
    query: Option<&str>,
    workspace_id_str: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<AuthenticatedDevice, AppError> {
    let ws_id = uuid::Uuid::parse_str(workspace_id_str).map_err(|_| AppError::not_found())?;

    // 1. Authorization: Bearer mcd_...
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or_else(AppError::unauthenticated)?;

    if !auth_header.starts_with(DEVICE_TOKEN_PREFIX) {
        return Err(AppError::unauthenticated());
    }

    let token_hash = sha256(auth_header.as_bytes());

    type DeviceRow = (uuid::Uuid, uuid::Uuid, String, String, String, String, bool);
    let dev_row: Option<DeviceRow> = sqlx::query_as(
        "SELECT id, workspace_id, machine_name, signing_key, recipient, status, bootstrap
         FROM devices WHERE token_hash = $1",
    )
    .bind(&token_hash)
    .fetch_optional(&state.db)
    .await?;

    let (dev_id, dev_ws_id, machine_name, signing_key, recipient, status_str, bootstrap) =
        dev_row.ok_or_else(AppError::unauthenticated)?;

    // Tenant isolation: token must belong to the workspace in the URL
    if dev_ws_id != ws_id {
        return Err(AppError::not_found());
    }
    // Transport revoked (e.g. its account was removed from the workspace).
    if status_str == "revoked" {
        return Err(AppError::unauthenticated());
    }

    // 2. X-Muster-Device must match device id
    let claimed_device = headers
        .get(HEADER_DEVICE_ID)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| uuid::Uuid::parse_str(v.trim()).ok())
        .ok_or_else(|| {
            AppError::new(
                StatusCode::UNAUTHORIZED,
                error_codes::INVALID_SIGNATURE,
                "missing or invalid X-Muster-Device header",
            )
        })?;

    if claimed_device != dev_id {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_SIGNATURE,
            "X-Muster-Device does not match bearer token identity",
        ));
    }

    // Per-machine budget, keyed by the identity the bearer token proved and
    // taken before any further work (nonce write, signature check).
    if let Err(retry) = state.rate_limits.device.check(&dev_id) {
        return Err(crate::rate_limit::limited(retry));
    }

    // 3. X-Muster-Timestamp must be within +/- 300s
    let timestamp_str = headers
        .get(HEADER_TIMESTAMP)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::UNAUTHORIZED,
                error_codes::INVALID_SIGNATURE,
                "missing X-Muster-Timestamp header",
            )
        })?;

    let timestamp: i64 = timestamp_str.parse().map_err(|_| {
        AppError::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_SIGNATURE,
            "invalid X-Muster-Timestamp; must be unix seconds",
        )
    })?;

    let now = OffsetDateTime::now_utc().unix_timestamp();
    if (now - timestamp).abs() > SIGNED_REQUEST_MAX_SKEW_SECS {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_SIGNATURE,
            "clock skew exceeds 300 seconds",
        ));
    }

    // 4. X-Muster-Nonce must be unique per device
    let nonce = headers
        .get(HEADER_NONCE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| v.len() >= 8)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::UNAUTHORIZED,
                error_codes::INVALID_SIGNATURE,
                "missing or short X-Muster-Nonce header",
            )
        })?;

    // Insert nonce atomically
    let nonce_inserted = sqlx::query(
        "INSERT INTO device_nonces (device_id, nonce) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(dev_id)
    .bind(nonce)
    .execute(&state.db)
    .await?;

    if nonce_inserted.rows_affected() == 0 {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_SIGNATURE,
            "replayed nonce",
        ));
    }

    // 5. X-Muster-Content-SHA256 must match body digest
    let content_sha = headers
        .get(HEADER_CONTENT_SHA256)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::UNAUTHORIZED,
                error_codes::INVALID_SIGNATURE,
                "missing X-Muster-Content-SHA256 header",
            )
        })?;

    let actual_sha = body_sha256_hex(body);
    if !content_sha.eq_ignore_ascii_case(&actual_sha) {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_SIGNATURE,
            "X-Muster-Content-SHA256 does not match body",
        ));
    }

    // 6. Base revision and idempotency key
    let base_revision = headers
        .get(HEADER_BASE_REVISION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or("");

    let idempotency_key = headers
        .get(HEADER_IDEMPOTENCY_KEY)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or("");

    // 7. Signature verification: X-Muster-Signature: v1=<hex>
    let sig_header = headers
        .get(HEADER_SIGNATURE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::UNAUTHORIZED,
                error_codes::INVALID_SIGNATURE,
                "missing X-Muster-Signature header",
            )
        })?;

    let hex_sig = sig_header.strip_prefix("v1=").ok_or_else(|| {
        AppError::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_SIGNATURE,
            "signature must start with v1=",
        )
    })?;

    let path_and_query = canonical_path_and_query(path, query);

    let signed_req = SignedRequestV1 {
        method: method.as_str(),
        path_and_query: &path_and_query,
        workspace_id: workspace_id_str,
        device_id: &dev_id.to_string(),
        body_sha256_hex: &actual_sha,
        base_revision,
        idempotency_key,
        timestamp: timestamp_str,
        nonce,
    };

    let canonical_bytes = canonical_request_v1(&signed_req);

    muster::signing::verify(&signing_key, &canonical_bytes, hex_sig).map_err(|_| {
        AppError::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_SIGNATURE,
            "signature verification failed",
        )
    })?;

    let status = match status_str.as_str() {
        "approved" => DeviceStatus::Approved,
        "pending" => DeviceStatus::Pending,
        "revoked" => DeviceStatus::Revoked,
        _ => DeviceStatus::Linked,
    };

    Ok(AuthenticatedDevice {
        id: dev_id,
        workspace_id: ws_id,
        machine_name,
        signing_key,
        recipient,
        status,
        bootstrap,
    })
}
