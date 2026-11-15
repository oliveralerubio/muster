//! Signed receipts: machines record applied fleet states.
//!
//! A receipt attests what the machine declares, not what its agents did.
//! Older offline receipts are accepted into history without displacing
//! newer ones.

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::auth::device_sig::authenticate_device_request;
use crate::error::AppError;
use crate::protocol::{canonical_receipt_v1, error_codes, Receipt, ReceiptV1, SubmitReceipt};
use crate::state::AppState;
use crate::tenancy::parse_json;

fn rfc3339(t: OffsetDateTime) -> Result<String, AppError> {
    t.format(&Rfc3339).map_err(AppError::internal)
}

/// `POST /v1/workspaces/{ws}/receipts`: stores a signed apply receipt.
pub async fn submit_receipt(
    State(state): State<AppState>,
    Path(ws_str): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Result<(StatusCode, Json<Receipt>), AppError> {
    let (parts, body_stream) = request.into_parts();
    let raw_query = parts.uri.query();
    let body = axum::body::to_bytes(body_stream, 1024 * 1024)
        .await
        .map_err(|_| AppError::bad_request("receipt body too large"))?;

    let device = authenticate_device_request(
        &state,
        &parts.method,
        parts.uri.path(),
        raw_query,
        &ws_str,
        &headers,
        &body,
    )
    .await?;

    let req: SubmitReceipt = parse_json(&body)?;

    if req.sequence == 0 {
        return Err(AppError::bad_request("sequence must be strictly positive"));
    }

    // Validate applied_at RFC 3339
    let applied_time = OffsetDateTime::parse(&req.applied_at, &Rfc3339)
        .map_err(|_| AppError::bad_request("applied_at must be an RFC 3339 timestamp"))?;

    // Verify receipt signature
    let receipt_v1 = ReceiptV1 {
        workspace_id: &device.workspace_id.to_string(),
        device_id: &device.id.to_string(),
        revision: &req.revision,
        epoch: req.epoch,
        sequence: req.sequence,
        outcome: req.outcome.as_str(),
        applied_at: &req.applied_at,
    };
    let canonical = canonical_receipt_v1(&receipt_v1);
    muster::signing::verify(&device.signing_key, &canonical, &req.signature).map_err(|_| {
        AppError::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_SIGNATURE,
            "invalid receipt signature",
        )
    })?;

    let mut tx = state.db.begin().await?;

    type ExistingRow = (String, i64, String, OffsetDateTime, OffsetDateTime, String);
    let existing: Option<ExistingRow> = sqlx::query_as(
        "SELECT revision, epoch, outcome, applied_at, received_at, signature
         FROM receipts WHERE workspace_id = $1 AND device_id = $2 AND sequence = $3",
    )
    .bind(device.workspace_id)
    .bind(device.id)
    .bind(req.sequence as i64)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((rev, ep, outcome_str, app_at, rec_at, sig)) = existing {
        let is_same = rev == req.revision
            && ep == req.epoch as i64
            && outcome_str == req.outcome.as_str()
            && app_at == applied_time
            && sig == req.signature;

        if !is_same {
            return Err(AppError::idempotency_mismatch());
        }

        let receipt = Receipt {
            device_id: device.id.to_string(),
            revision: req.revision,
            epoch: req.epoch,
            sequence: req.sequence,
            outcome: req.outcome,
            applied_at: rfc3339(app_at)?,
            received_at: rfc3339(rec_at)?,
            signature: sig,
        };
        tx.commit().await?;
        return Ok((StatusCode::OK, Json(receipt)));
    }

    // PostgreSQL keeps microseconds; truncate so the first response and
    // every idempotent replay report the same received_at.
    let now = OffsetDateTime::now_utc();
    let now = now
        .replace_nanosecond(now.nanosecond() / 1_000 * 1_000)
        .map_err(AppError::internal)?;
    sqlx::query(
        "INSERT INTO receipts (workspace_id, device_id, sequence, revision, epoch, outcome, applied_at, received_at, signature)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(device.workspace_id)
    .bind(device.id)
    .bind(req.sequence as i64)
    .bind(&req.revision)
    .bind(req.epoch as i64)
    .bind(req.outcome.as_str())
    .bind(applied_time)
    .bind(now)
    .bind(&req.signature)
    .execute(&mut *tx)
    .await?;

    sqlx::query("UPDATE devices SET last_seen_at = now() WHERE id = $1")
        .bind(device.id)
        .execute(&mut *tx)
        .await?;

    let receipt = Receipt {
        device_id: device.id.to_string(),
        revision: req.revision,
        epoch: req.epoch,
        sequence: req.sequence,
        outcome: req.outcome,
        applied_at: req.applied_at,
        received_at: rfc3339(now)?,
        signature: req.signature,
    };

    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(receipt)))
}
