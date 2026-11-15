//! Retained proposals: CAS conflicts, unquorate governance changes, and
//! requests for local signing (enrollment, restore).
//!
//! The service never signs governance or restores files itself: an
//! authorized local client reviews the proposal, applies it locally with
//! its own keys and quorum rules, publishes the result via `POST /sync`,
//! and marks the proposal resolved.

use axum::extract::{FromRequestParts, Path, State};
use axum::http::header::HeaderValue;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::account::Account;
use crate::auth::device_sig::{authenticate_device_request, AuthenticatedDevice};
use crate::error::AppError;
use crate::protocol::{
    error_codes, scopes, CreateProposal, Proposal, ProposalKind, ProposalStatus, ResolveProposal,
    GIT_BUNDLE_MEDIA_TYPE,
};
use crate::state::AppState;
use crate::tenancy::parse_json;

fn rfc3339(t: OffsetDateTime) -> Result<String, AppError> {
    t.format(&Rfc3339).map_err(AppError::internal)
}

pub enum Caller {
    Account(Account),
    Device(AuthenticatedDevice),
}

pub async fn extract_caller(
    state: &AppState,
    method: &Method,
    path: &str,
    query: Option<&str>,
    ws_str: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Caller, AppError> {
    let ws = Uuid::parse_str(ws_str).map_err(|_| AppError::not_found())?;
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);

    if let Some(token) = auth {
        if token.starts_with(crate::protocol::DEVICE_TOKEN_PREFIX) {
            let device =
                authenticate_device_request(state, method, path, query, ws_str, headers, body)
                    .await?;
            return Ok(Caller::Device(device));
        }
    }

    let (mut parts, _) = axum::extract::Request::new(()).into_parts();
    parts.headers = headers.clone();
    parts.method = method.clone();
    let account = Account::from_request_parts(&mut parts, state).await?;
    account.require_scope(scopes::WORKSPACES_READ)?;
    let member: Option<Uuid> = sqlx::query_scalar(
        "SELECT workspace_id FROM memberships WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(ws)
    .bind(account.user_id)
    .fetch_optional(&state.db)
    .await?;
    member.ok_or_else(AppError::not_found)?;
    Ok(Caller::Account(account))
}

/// Whether `key` is an admin or writer of the accepted signed governance.
async fn governance_authorizes(state: &AppState, ws: Uuid, key: &str) -> Result<bool, AppError> {
    let row: Option<(serde_json::Value,)> =
        sqlx::query_as("SELECT anchor FROM workspace_heads WHERE workspace_id = $1")
            .bind(ws)
            .fetch_optional(&state.db)
            .await?;
    let Some((anchor,)) = row else {
        return Ok(false);
    };
    let anchor: muster::fleet_state::TrustAnchor =
        serde_json::from_value(anchor).map_err(AppError::internal)?;
    Ok(anchor.admins.iter().any(|k| k == key) || anchor.writers.iter().any(|k| k == key))
}

/// Requests the cloud role allows to make or close a review request: an
/// owner/admin account (browser session) or a machine the signed
/// governance authorizes. Neither signs anything.
async fn require_reviewer(
    state: &AppState,
    ws: Uuid,
    caller: &Caller,
    what: &str,
) -> Result<(), AppError> {
    match caller {
        Caller::Device(dev) => {
            if governance_authorizes(state, ws, &dev.signing_key).await? {
                Ok(())
            } else {
                Err(AppError::new(
                    StatusCode::FORBIDDEN,
                    error_codes::DEVICE_NOT_APPROVED,
                    format!("the accepted governance does not authorize this machine to {what}"),
                ))
            }
        }
        Caller::Account(account) => {
            account.require_scope(scopes::WORKSPACES_WRITE)?;
            let role: Option<String> = sqlx::query_scalar(
                "SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2",
            )
            .bind(ws)
            .bind(account.user_id)
            .fetch_optional(&state.db)
            .await?;
            match role.as_deref() {
                Some("owner") | Some("admin") => Ok(()),
                _ => Err(AppError::new(
                    StatusCode::FORBIDDEN,
                    error_codes::FORBIDDEN_ROLE,
                    format!("only owners and admins may {what}"),
                )),
            }
        }
    }
}

fn valid_restore_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.contains('\0')
        && p.split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

type ProposalRow = (
    Uuid,
    Uuid,
    Option<Uuid>,
    Option<Uuid>,
    String,
    String,
    String,
    String,
    Option<i64>,
    Option<serde_json::Value>,
    OffsetDateTime,
    Option<String>,
    Option<OffsetDateTime>,
);

const SELECT_PROPOSAL: &str =
    "SELECT id, workspace_id, device_id, created_by, kind, status, base_revision, revision, epoch, payload, created_at, result_revision, resolved_at FROM proposals";

fn to_dto(
    (
        id,
        _ws,
        dev,
        by,
        kind,
        status,
        base,
        rev,
        epoch,
        payload,
        created,
        res_rev,
        res_at,
    ): ProposalRow,
) -> Result<Proposal, AppError> {
    let kind = match kind.as_str() {
        "conflict" => ProposalKind::Conflict,
        "pending_quorum" => ProposalKind::PendingQuorum,
        "enrollment" => ProposalKind::Enrollment,
        "restore" => ProposalKind::Restore,
        _ => ProposalKind::Unauthorized,
    };
    let status = match status.as_str() {
        "resolved" => ProposalStatus::Resolved,
        "rejected" => ProposalStatus::Rejected,
        "superseded" => ProposalStatus::Superseded,
        _ => ProposalStatus::Open,
    };
    Ok(Proposal {
        id: id.to_string(),
        kind,
        status,
        base_revision: base,
        revision: rev,
        epoch: epoch.map(|e| e as u64),
        device_id: dev.map(|d| d.to_string()),
        created_by: by.map(|u| u.to_string()),
        payload,
        created_at: rfc3339(created)?,
        result_revision: res_rev,
        resolved_at: res_at.map(rfc3339).transpose()?,
    })
}

/// `GET /v1/workspaces/{ws}/proposals`: newest first.
pub async fn list(
    State(state): State<AppState>,
    Path(ws_str): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Vec<Proposal>>, AppError> {
    let caller = extract_caller(
        &state,
        &method,
        uri.path(),
        uri.query(),
        &ws_str,
        &headers,
        &[],
    )
    .await?;
    let ws = Uuid::parse_str(&ws_str).map_err(|_| AppError::not_found())?;
    let _ = caller;

    let rows: Vec<ProposalRow> = sqlx::query_as(&format!(
        "{SELECT_PROPOSAL} WHERE workspace_id = $1 ORDER BY created_at DESC, id DESC"
    ))
    .bind(ws)
    .fetch_all(&state.db)
    .await?;

    rows.into_iter()
        .map(to_dto)
        .collect::<Result<Vec<_>, _>>()
        .map(Json)
}

/// `GET /v1/workspaces/{ws}/proposals/{id}`.
pub async fn detail(
    State(state): State<AppState>,
    Path((ws_str, id_str)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Proposal>, AppError> {
    let caller = extract_caller(
        &state,
        &method,
        uri.path(),
        uri.query(),
        &ws_str,
        &headers,
        &[],
    )
    .await?;
    let ws = Uuid::parse_str(&ws_str).map_err(|_| AppError::not_found())?;
    let id = Uuid::parse_str(&id_str).map_err(|_| AppError::not_found())?;
    let _ = caller;

    let row: Option<ProposalRow> = sqlx::query_as(&format!(
        "{SELECT_PROPOSAL} WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(ws)
    .bind(id)
    .fetch_optional(&state.db)
    .await?;

    to_dto(row.ok_or_else(AppError::not_found)?).map(Json)
}

/// `GET /v1/workspaces/{ws}/proposals/{id}/bundle`.
pub async fn bundle(
    State(state): State<AppState>,
    Path((ws_str, id_str)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let caller = extract_caller(
        &state,
        &method,
        uri.path(),
        uri.query(),
        &ws_str,
        &headers,
        &[],
    )
    .await?;
    let ws = Uuid::parse_str(&ws_str).map_err(|_| AppError::not_found())?;
    let id = Uuid::parse_str(&id_str).map_err(|_| AppError::not_found())?;
    let _ = caller;

    let row: Option<(bool,)> =
        sqlx::query_as("SELECT has_bundle FROM proposals WHERE workspace_id = $1 AND id = $2")
            .bind(ws)
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let (has_bundle,) = row.ok_or_else(AppError::not_found)?;
    if !has_bundle {
        return Err(AppError::not_found());
    }

    let repos = state.repos.clone();
    let bytes = tokio::task::spawn_blocking(move || repos.upload_bundle(ws, id))
        .await
        .map_err(AppError::internal)??;

    let mut resp = (StatusCode::OK, bytes).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(GIT_BUNDLE_MEDIA_TYPE),
    );
    Ok(resp)
}

/// Bytes a proposal payload adds to storage use, computed by PostgreSQL
/// with the same expression `billing::storage_used` counts with.
async fn stored_size(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    payload: &serde_json::Value,
) -> Result<u64, AppError> {
    let size: i64 = sqlx::query_scalar("SELECT octet_length(($1::jsonb)::text)::int8")
        .bind(payload)
        .fetch_one(&mut **tx)
        .await?;
    Ok(size.max(0) as u64)
}

/// `POST /v1/workspaces/{ws}/proposals`: creates a declarative proposal
/// (enrollment request, restore).
pub async fn create(
    State(state): State<AppState>,
    Path(ws_str): Path<String>,
    headers: HeaderMap,
    request: axum::extract::Request,
) -> Result<(StatusCode, Json<Proposal>), AppError> {
    let ws = Uuid::parse_str(&ws_str).map_err(|_| AppError::not_found())?;
    let (parts, body_stream) = request.into_parts();
    let raw_query = parts.uri.query();
    let body = axum::body::to_bytes(body_stream, 1024 * 1024)
        .await
        .map_err(|_| AppError::bad_request("body too large"))?;

    let caller = extract_caller(
        &state,
        &parts.method,
        parts.uri.path(),
        raw_query,
        &ws_str,
        &headers,
        &body,
    )
    .await?;

    let req: CreateProposal = parse_json(&body)?;

    match req.kind {
        ProposalKind::Enrollment => {
            let Caller::Device(ref dev) = caller else {
                return Err(AppError::bad_request(
                    "enrollment proposals must be signed by the machine",
                ));
            };

            // Payload validation: must carry the requesting machine's public key.
            let pk = req.payload.get("signing_key").and_then(|v| v.as_str());
            if pk != Some(&dev.signing_key) {
                return Err(AppError::bad_request(
                    "enrollment request signing_key must match the requesting device",
                ));
            }
            let request: muster::fleet::EnrollmentRequest =
                serde_json::from_value(req.payload.clone()).map_err(|e| {
                    AppError::bad_request(format!("payload is not an enrollment request: {e}"))
                })?;
            if !request.approvals.is_empty() {
                return Err(AppError::bad_request(
                    "enrollment proposals cannot include approvals",
                ));
            }
            if request.recipient != dev.recipient || request.nonce.trim().is_empty() {
                return Err(AppError::bad_request(
                    "enrollment request must carry this device's recipient and a nonce",
                ));
            }

            // Check if there is already an open enrollment proposal for this device.
            let existing: Option<ProposalRow> = sqlx::query_as(&format!(
                "{SELECT_PROPOSAL} WHERE device_id = $1 AND kind = 'enrollment' AND status = 'open'"
            ))
            .bind(dev.id)
            .fetch_optional(&state.db)
            .await?;

            if let Some(row) = existing {
                return Ok((StatusCode::OK, Json(to_dto(row)?)));
            }

            let mut tx = state.db.begin().await?;
            let size = stored_size(&mut tx, &req.payload).await?;
            crate::billing::ensure_storage_fits(&state, &mut tx, ws, size).await?;
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO proposals (id, workspace_id, device_id, kind, status, base_revision, revision, epoch, payload, has_bundle)
                 VALUES ($1, $2, $3, 'enrollment', 'open', $4, '', NULL, $5, false)",
            )
            .bind(id)
            .bind(ws)
            .bind(dev.id)
            .bind(&req.base_revision)
            .bind(&req.payload)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;

            let row: ProposalRow = sqlx::query_as(&format!("{SELECT_PROPOSAL} WHERE id = $1"))
                .bind(id)
                .fetch_one(&state.db)
                .await?;

            Ok((StatusCode::CREATED, Json(to_dto(row)?)))
        }
        ProposalKind::Restore => {
            require_reviewer(&state, ws, &caller, "request a restore").await?;
            let payload: crate::protocol::RestorePayload =
                serde_json::from_value(req.payload.clone())
                    .map_err(|e| AppError::bad_request(format!("invalid restore payload: {e}")))?;
            if payload.paths.is_empty() || !payload.paths.iter().all(|p| valid_restore_path(p)) {
                return Err(AppError::bad_request(
                    "paths must be non-empty relative repository paths",
                )
                .with_details(serde_json::json!({ "field": "paths" })));
            }
            let accepted: Option<String> = sqlx::query_scalar(
                "SELECT revision FROM accepted_revisions WHERE workspace_id = $1 AND revision = $2",
            )
            .bind(ws)
            .bind(&payload.target_revision)
            .fetch_optional(&state.db)
            .await?;
            if accepted.is_none() {
                return Err(
                    AppError::bad_request("target_revision is not an accepted revision")
                        .with_details(serde_json::json!({ "field": "target_revision" })),
                );
            }

            let mut tx = state.db.begin().await?;
            let size = stored_size(&mut tx, &req.payload).await?;
            crate::billing::ensure_storage_fits(&state, &mut tx, ws, size).await?;
            let id = Uuid::new_v4();
            let (dev_id, user_id) = match &caller {
                Caller::Device(d) => (Some(d.id), None),
                Caller::Account(a) => (None, Some(a.user_id)),
            };

            sqlx::query(
                "INSERT INTO proposals (id, workspace_id, device_id, created_by, kind, status, base_revision, revision, epoch, payload, has_bundle)
                 VALUES ($1, $2, $3, $4, 'restore', 'open', $5, '', NULL, $6, false)",
            )
            .bind(id)
            .bind(ws)
            .bind(dev_id)
            .bind(user_id)
            .bind(&req.base_revision)
            .bind(&req.payload)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;

            let row: ProposalRow = sqlx::query_as(&format!("{SELECT_PROPOSAL} WHERE id = $1"))
                .bind(id)
                .fetch_one(&state.db)
                .await?;

            Ok((StatusCode::CREATED, Json(to_dto(row)?)))
        }
        _ => Err(AppError::bad_request(
            "unsupported proposal kind for declarative POST",
        )),
    }
}

/// `POST /v1/workspaces/{ws}/proposals/{id}/resolve`: marks a proposal
/// resolved after a client signed and published the incorporating commit.
pub async fn resolve(
    State(state): State<AppState>,
    Path((ws_str, id_str)): Path<(String, String)>,
    headers: HeaderMap,
    request: axum::extract::Request,
) -> Result<Json<Proposal>, AppError> {
    let ws = Uuid::parse_str(&ws_str).map_err(|_| AppError::not_found())?;
    let id = Uuid::parse_str(&id_str).map_err(|_| AppError::not_found())?;
    let (parts, body_stream) = request.into_parts();
    let raw_query = parts.uri.query();
    let body = axum::body::to_bytes(body_stream, 1024 * 1024)
        .await
        .map_err(|_| AppError::bad_request("body too large"))?;

    let caller = extract_caller(
        &state,
        &parts.method,
        parts.uri.path(),
        raw_query,
        &ws_str,
        &headers,
        &body,
    )
    .await?;

    require_reviewer(&state, ws, &caller, "resolve proposals").await?;

    let req: ResolveProposal = parse_json(&body)?;

    let mut tx = state.db.begin().await?;

    let row: Option<ProposalRow> = sqlx::query_as(&format!(
        "{SELECT_PROPOSAL} WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    ))
    .bind(ws)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;

    let (prop_id, _, _, _, _, status_str, base_rev, _, _, _, _, _, _) =
        row.ok_or_else(AppError::not_found)?;

    if status_str != "open" {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            error_codes::PROPOSAL_CLOSED,
            "this proposal is already closed",
        ));
    }

    let new_status = if let Some(ref res_rev) = req.result_revision {
        // Result revision must be an accepted revision in this workspace.
        let rev_exists: Option<(String,)> = sqlx::query_as(
            "SELECT revision FROM accepted_revisions WHERE workspace_id = $1 AND revision = $2",
        )
        .bind(ws)
        .bind(res_rev)
        .fetch_optional(&mut *tx)
        .await?;
        if rev_exists.is_none() {
            return Err(AppError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                error_codes::INVALID_HISTORY,
                "result_revision is not an accepted revision of this workspace",
            ));
        }
        // It must incorporate the state the proposal was based on.
        if base_rev.len() == 40 {
            let repos = state.repos.clone();
            let (b, r) = (base_rev.clone(), res_rev.clone());
            let descends = tokio::task::spawn_blocking(move || repos.is_ancestor(ws, &b, &r))
                .await
                .map_err(AppError::internal)??;
            if !descends {
                return Err(AppError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    error_codes::INVALID_HISTORY,
                    "result_revision does not descend from the proposal's base revision",
                ));
            }
        }
        "resolved"
    } else {
        "rejected"
    };

    let (dev_id, user_id) = match &caller {
        Caller::Device(d) => (Some(d.id), None),
        Caller::Account(a) => (None, Some(a.user_id)),
    };

    sqlx::query(
        "UPDATE proposals
         SET status = $1, result_revision = $2, resolved_at = now(), resolved_by_device = $3, resolved_by_user = $4
         WHERE id = $5",
    )
    .bind(new_status)
    .bind(&req.result_revision)
    .bind(dev_id)
    .bind(user_id)
    .bind(prop_id)
    .execute(&mut *tx)
    .await?;

    let updated: ProposalRow = sqlx::query_as(&format!("{SELECT_PROPOSAL} WHERE id = $1"))
        .bind(prop_id)
        .fetch_one(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(Json(to_dto(updated)?))
}
