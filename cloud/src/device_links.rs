//! Workspace machine enrollment and cloud link management.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::auth::account::Account;
use crate::auth::{random_token, sha256};
use crate::error::AppError;
use crate::protocol::{
    canonical_link_v1, error_codes, scopes, Device, DeviceStatus, HeadRef, LinkDeviceRequest,
    LinkedDevice, Receipt, ReceiptOutcome, DEVICE_TOKEN_PREFIX,
};
use crate::state::AppState;
use crate::tenancy::{idempotency_key, parse_json};

fn rfc3339(t: OffsetDateTime) -> Result<String, AppError> {
    t.format(&Rfc3339).map_err(AppError::internal)
}

fn validate_machine_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err(AppError::bad_request(
            "machine_name must be 1-64 ASCII alphanumeric characters or ._-",
        )
        .with_details(serde_json::json!({ "field": "machine_name" })));
    }
    Ok(name.to_string())
}

/// `POST /v1/workspaces/{ws}/devices`: links a machine to a workspace.
pub async fn link_device(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<LinkedDevice>), AppError> {
    account.require_scope(scopes::DEVICES_LINK)?;
    let key = idempotency_key(&headers)?;
    let ws_id = uuid::Uuid::parse_str(&ws_str).map_err(|_| AppError::not_found())?;

    let req: LinkDeviceRequest = parse_json(&body)?;
    let machine_name = validate_machine_name(&req.machine_name)?;

    // Validate signing key format and parse verifying key
    muster::signing::parse_public(&req.signing_key).map_err(|_| {
        AppError::bad_request("invalid signing_key format; must be musterpk1<hex>")
            .with_details(serde_json::json!({ "field": "signing_key" }))
    })?;

    if !req.recipient.starts_with("age1") || req.recipient.len() < 5 {
        return Err(
            AppError::bad_request("invalid recipient format; must be age1...")
                .with_details(serde_json::json!({ "field": "recipient" })),
        );
    }

    // Verify proof of possession over canonical_link_v1
    let canonical_msg = canonical_link_v1(
        &ws_id.to_string(),
        &account.user_id.to_string(),
        &machine_name,
        &req.signing_key,
        &req.recipient,
    );
    muster::signing::verify(&req.signing_key, &canonical_msg, &req.proof).map_err(|_| {
        AppError::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_SIGNATURE,
            "invalid proof signature for this machine and workspace",
        )
    })?;

    let body_hash = sha256(&body);
    let path = format!("/v1/workspaces/{ws_id}/devices");

    let mut tx = state.db.begin().await?;

    // Verify caller's membership in the workspace
    let member_role: Option<String> =
        sqlx::query_scalar("SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2")
            .bind(ws_id)
            .bind(account.user_id)
            .fetch_optional(&mut *tx)
            .await?;

    let role = member_role.ok_or_else(AppError::not_found)?;

    // Serialises links (and the bootstrap claim) per workspace.
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(ws_id)
        .execute(&mut *tx)
        .await?;

    // Lock on idempotency key for this user & workspace
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text), hashtext($2::text))")
        .bind(account.user_id.to_string())
        .bind(key.to_string())
        .execute(&mut *tx)
        .await?;

    let prev: Option<(String, String, Vec<u8>, i32, serde_json::Value)> = sqlx::query_as(
        "SELECT method, path, body_sha256, status, response FROM account_idempotency
         WHERE user_id = $1 AND idem_key = $2",
    )
    .bind(account.user_id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((method, prev_path, prev_hash, status, response)) = prev {
        if method != "POST" || prev_path != path || prev_hash != body_hash {
            return Err(AppError::idempotency_mismatch());
        }
        let status = StatusCode::from_u16(status as u16).map_err(AppError::internal)?;
        let device: LinkedDevice = serde_json::from_value(response).map_err(AppError::internal)?;
        return Ok((status, Json(device)));
    }

    // Check existing head
    let head_row: Option<(String, i64)> =
        sqlx::query_as("SELECT head_revision, epoch FROM workspace_heads WHERE workspace_id = $1")
            .bind(ws_id)
            .fetch_optional(&mut *tx)
            .await?;

    let has_history = head_row.is_some();
    let head = head_row.map(|(rev, ep)| HeadRef {
        revision: rev,
        epoch: ep as u64,
    });

    // Check if signing key is already linked in this workspace
    let existing_by_key: Option<(uuid::Uuid, uuid::Uuid, bool, String)> = sqlx::query_as(
        "SELECT id, linked_by, bootstrap, status FROM devices WHERE workspace_id = $1 AND signing_key = $2 FOR UPDATE",
    )
    .bind(ws_id)
    .bind(&req.signing_key)
    .fetch_optional(&mut *tx)
    .await?;

    let device_token = format!("{DEVICE_TOKEN_PREFIX}{}", random_token());
    let token_hash = sha256(device_token.as_bytes());

    let (device_id, bootstrap, status) = if let Some((
        dev_id,
        linked_by,
        is_bootstrap,
        status_str,
    )) = existing_by_key
    {
        if linked_by != account.user_id {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                error_codes::DEVICE_CONFLICT,
                "signing key is already linked in this workspace by another account",
            ));
        }
        // Rotate token
        sqlx::query(
            "UPDATE devices SET token_hash = $1, machine_name = $2, recipient = $3 WHERE id = $4",
        )
        .bind(&token_hash)
        .bind(&machine_name)
        .bind(&req.recipient)
        .bind(dev_id)
        .execute(&mut *tx)
        .await?;

        let status = match status_str.as_str() {
            "approved" => DeviceStatus::Approved,
            "pending" => DeviceStatus::Pending,
            "revoked" => DeviceStatus::Revoked,
            _ => DeviceStatus::Linked,
        };
        (dev_id, is_bootstrap, status)
    } else {
        // Check machine name collision
        let name_taken: Option<uuid::Uuid> = sqlx::query_scalar(
            "SELECT id FROM devices WHERE workspace_id = $1 AND machine_name = $2",
        )
        .bind(ws_id)
        .bind(&machine_name)
        .fetch_optional(&mut *tx)
        .await?;

        if name_taken.is_some() {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                error_codes::DEVICE_CONFLICT,
                "machine name is already in use in this workspace",
            ));
        }

        // The declared machine limit; the workspace row lock above makes the
        // count and the insert atomic against concurrent links.
        let limits = crate::billing::workspace_limits(&state.config, &mut *tx, ws_id).await?;
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM devices WHERE workspace_id = $1 AND status <> 'revoked'",
        )
        .bind(ws_id)
        .fetch_one(&mut *tx)
        .await?;
        if active >= i64::from(limits.max_devices) {
            return Err(AppError::new(
                StatusCode::INSUFFICIENT_STORAGE,
                error_codes::QUOTA_EXCEEDED,
                "the workspace plan's machine limit is reached",
            )
            .with_details(serde_json::json!({
                "limit": "max_devices",
                "max_devices": limits.max_devices,
            })));
        }

        // Atomic bootstrap check: if empty workspace and caller is owner, check if any device holds bootstrap
        let is_bootstrap = if !has_history && role == "owner" {
            let existing_bootstrap: Option<uuid::Uuid> = sqlx::query_scalar(
                "SELECT id FROM devices WHERE workspace_id = $1 AND bootstrap = true FOR UPDATE",
            )
            .bind(ws_id)
            .fetch_optional(&mut *tx)
            .await?;
            existing_bootstrap.is_none()
        } else {
            false
        };

        let new_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO devices (id, workspace_id, machine_name, signing_key, recipient, token_hash, status, bootstrap, linked_by)
             VALUES ($1, $2, $3, $4, $5, $6, 'linked', $7, $8)",
        )
        .bind(new_id)
        .bind(ws_id)
        .bind(&machine_name)
        .bind(&req.signing_key)
        .bind(&req.recipient)
        .bind(&token_hash)
        .bind(is_bootstrap)
        .bind(account.user_id)
        .execute(&mut *tx)
        .await?;

        (new_id, is_bootstrap, DeviceStatus::Linked)
    };

    let result = LinkedDevice {
        device_id: device_id.to_string(),
        workspace_id: ws_id.to_string(),
        device_token,
        status,
        bootstrap,
        head,
    };

    let response_val = serde_json::to_value(&result).map_err(AppError::internal)?;
    sqlx::query(
        "INSERT INTO account_idempotency (user_id, idem_key, method, path, body_sha256, status, response)
         VALUES ($1, $2, 'POST', $3, $4, 201, $5)",
    )
    .bind(account.user_id)
    .bind(key)
    .bind(path)
    .bind(&body_hash)
    .bind(&response_val)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(result)))
}

/// `GET /v1/workspaces/{ws}/devices`: lists devices in a workspace.
pub async fn list_devices(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
) -> Result<Json<Vec<Device>>, AppError> {
    account.require_scope(scopes::WORKSPACES_READ)?;
    let ws_id = uuid::Uuid::parse_str(&ws_str).map_err(|_| AppError::not_found())?;

    // Check membership
    let is_member: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT workspace_id FROM memberships WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(ws_id)
    .bind(account.user_id)
    .fetch_optional(&state.db)
    .await?;

    if is_member.is_none() {
        return Err(AppError::not_found());
    }

    type DeviceRow = (
        uuid::Uuid,
        String,
        String,
        String,
        String,
        bool,
        uuid::Uuid,
        OffsetDateTime,
        Option<OffsetDateTime>,
    );

    let rows: Vec<DeviceRow> = sqlx::query_as(
        "SELECT id, machine_name, signing_key, recipient, status, bootstrap, linked_by, created_at, last_seen_at
         FROM devices WHERE workspace_id = $1 ORDER BY created_at ASC, id ASC",
    )
    .bind(ws_id)
    .fetch_all(&state.db)
    .await?;

    let mut devices = Vec::new();
    for (
        id,
        machine_name,
        signing_key,
        recipient,
        status_str,
        bootstrap,
        linked_by,
        created_at,
        last_seen_at,
    ) in rows
    {
        let status = match status_str.as_str() {
            "approved" => DeviceStatus::Approved,
            "pending" => DeviceStatus::Pending,
            "revoked" => DeviceStatus::Revoked,
            _ => DeviceStatus::Linked,
        };

        // Query last receipt for this device
        let receipt_row: Option<(
            String,
            i64,
            i64,
            String,
            OffsetDateTime,
            OffsetDateTime,
            String,
        )> = sqlx::query_as(
            "SELECT revision, epoch, sequence, outcome, applied_at, received_at, signature
             FROM receipts WHERE device_id = $1 ORDER BY sequence DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&state.db)
        .await?;

        let last_receipt = match receipt_row {
            Some((rev, ep, seq, outcome_str, app_at, rec_at, sig)) => {
                let outcome = if outcome_str == "applied" {
                    ReceiptOutcome::Applied
                } else {
                    ReceiptOutcome::Failed
                };
                Some(Receipt {
                    device_id: id.to_string(),
                    revision: rev,
                    epoch: ep as u64,
                    sequence: seq as u64,
                    outcome,
                    applied_at: rfc3339(app_at)?,
                    received_at: rfc3339(rec_at)?,
                    signature: sig,
                })
            }
            None => None,
        };

        devices.push(Device {
            id: id.to_string(),
            machine_name,
            signing_key,
            recipient,
            status,
            bootstrap,
            linked_by: linked_by.to_string(),
            created_at: rfc3339(created_at)?,
            last_seen_at: last_seen_at.map(rfc3339).transpose()?,
            last_receipt,
        });
    }

    Ok(Json(devices))
}
