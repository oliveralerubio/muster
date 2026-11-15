//! Workspaces and memberships.
//!
//! Every workspace read goes through [`membership`], which only returns a row
//! when the caller is a member; anything else, including a well-formed id of
//! another tenant's workspace, is `404 not_found`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::auth::session::Session;
use crate::error::AppError;
use crate::protocol::{CreateWorkspace, PlanStatus, Role, Workspace, WORKSPACE_NAME_MAX_CHARS};
use crate::state::AppState;

/// Length of the free trial granted at workspace creation (no card).
pub const TRIAL_DAYS: i32 = 14;

type WorkspaceRow = (
    uuid::Uuid,
    String,
    String,
    OffsetDateTime,
    OffsetDateTime,
    OffsetDateTime,
);

const SELECT_FOR_MEMBER: &str = "SELECT w.id, w.name, m.role, w.created_at, w.trial_ends_at, now()
     FROM workspaces w JOIN memberships m ON m.workspace_id = w.id AND m.user_id = $1";

fn rfc3339(t: OffsetDateTime) -> Result<String, AppError> {
    t.format(&Rfc3339).map_err(AppError::internal)
}

fn to_dto(
    (id, name, role, created_at, trial_ends_at, now): WorkspaceRow,
) -> Result<Workspace, AppError> {
    let role =
        Role::parse(&role).ok_or_else(|| AppError::internal(format!("unknown role {role:?}")))?;
    // Paid plans arrive with billing (tramo 4); until then only the trial exists.
    let plan_status = if now < trial_ends_at {
        PlanStatus::Trialing
    } else {
        PlanStatus::TrialExpired
    };
    Ok(Workspace {
        id: id.to_string(),
        name,
        role,
        plan_status,
        trial_ends_at: rfc3339(trial_ends_at)?,
        created_at: rfc3339(created_at)?,
    })
}

fn validate_name(raw: &str) -> Result<String, AppError> {
    let name = raw.trim();
    let chars = name.chars().count();
    if chars == 0 || chars > WORKSPACE_NAME_MAX_CHARS || name.chars().any(char::is_control) {
        return Err(AppError::bad_request(format!(
            "name must be 1-{WORKSPACE_NAME_MAX_CHARS} characters without control characters"
        ))
        .with_details(serde_json::json!({ "field": "name" })));
    }
    Ok(name.to_string())
}

/// Parses a JSON body into `T`, mapping failures to `400 bad_request`.
pub fn parse_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, AppError> {
    serde_json::from_slice(body)
        .map_err(|err| AppError::bad_request(format!("invalid JSON body: {err}")))
}

/// Reads the mandatory `Idempotency-Key` header (a UUID).
pub fn idempotency_key(headers: &axum::http::HeaderMap) -> Result<uuid::Uuid, AppError> {
    headers
        .get(crate::protocol::HEADER_IDEMPOTENCY_KEY)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| uuid::Uuid::parse_str(v.trim()).ok())
        .ok_or_else(|| {
            AppError::bad_request("Idempotency-Key header with a UUID is required")
                .with_details(serde_json::json!({ "header": "Idempotency-Key" }))
        })
}

/// `POST /v1/workspaces`. Idempotent per (account, `Idempotency-Key`): a
/// retry with the same method, path and body bytes replays the original
/// response; anything else under that key is `422 idempotency_mismatch`.
pub async fn create(
    State(state): State<AppState>,
    session: crate::auth::account::Account,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    session.require_scope(crate::protocol::scopes::WORKSPACES_WRITE)?;
    let key = idempotency_key(&headers)?;
    let req: CreateWorkspace = parse_json(&body)?;
    let name = validate_name(&req.name)?;
    let body_hash = crate::auth::sha256(&body);
    let path = crate::protocol::routes::WORKSPACES;

    let mut tx = state.db.begin().await?;
    // Serialises concurrent requests carrying the same key for this account.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text), hashtext($2::text))")
        .bind(session.user_id.to_string())
        .bind(key.to_string())
        .execute(&mut *tx)
        .await?;
    let previous: Option<(String, String, Vec<u8>, i32, serde_json::Value)> = sqlx::query_as(
        "SELECT method, path, body_sha256, status, response FROM account_idempotency
         WHERE user_id = $1 AND idem_key = $2",
    )
    .bind(session.user_id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((method, prev_path, prev_hash, status, response)) = previous {
        if method != "POST" || prev_path != path || prev_hash != body_hash {
            return Err(AppError::idempotency_mismatch());
        }
        let status = StatusCode::from_u16(status as u16).map_err(AppError::internal)?;
        return Ok((status, Json(response)));
    }

    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workspaces (id, name, created_by, created_at, trial_ends_at)
         VALUES ($1, $2, $3, now(), now() + make_interval(days => $4))",
    )
    .bind(id)
    .bind(&name)
    .bind(session.user_id)
    .bind(TRIAL_DAYS)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO memberships (workspace_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(id)
        .bind(session.user_id)
        .execute(&mut *tx)
        .await?;
    let row: WorkspaceRow = sqlx::query_as(&format!("{SELECT_FOR_MEMBER} WHERE w.id = $2"))
        .bind(session.user_id)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let response = serde_json::to_value(to_dto(row)?).map_err(AppError::internal)?;
    sqlx::query(
        "INSERT INTO account_idempotency (user_id, idem_key, method, path, body_sha256, status, response)
         VALUES ($1, $2, 'POST', $3, $4, 201, $5)",
    )
    .bind(session.user_id)
    .bind(key)
    .bind(path)
    .bind(&body_hash)
    .bind(&response)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(response)))
}

/// `GET /v1/workspaces`: workspaces the caller is a member of, oldest first.
pub async fn list(
    State(state): State<AppState>,
    account: crate::auth::account::Account,
) -> Result<Json<Vec<Workspace>>, AppError> {
    account.require_scope(crate::protocol::scopes::WORKSPACES_READ)?;
    let rows: Vec<WorkspaceRow> =
        sqlx::query_as(&format!("{SELECT_FOR_MEMBER} ORDER BY w.created_at, w.id"))
            .bind(account.user_id)
            .fetch_all(&state.db)
            .await?;
    rows.into_iter()
        .map(to_dto)
        .collect::<Result<_, _>>()
        .map(Json)
}

/// The caller's view of workspace `raw_id`, or `404` when it does not exist,
/// is malformed, or the caller is not a member.
pub async fn membership(
    state: &AppState,
    session: &Session,
    raw_id: &str,
) -> Result<Workspace, AppError> {
    let id = uuid::Uuid::parse_str(raw_id).map_err(|_| AppError::not_found())?;
    let row: Option<WorkspaceRow> = sqlx::query_as(&format!("{SELECT_FOR_MEMBER} WHERE w.id = $2"))
        .bind(session.user_id)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    to_dto(row.ok_or_else(AppError::not_found)?)
}

/// `GET /v1/workspaces/{ws}`.
pub async fn detail(
    State(state): State<AppState>,
    account: crate::auth::account::Account,
    Path(ws): Path<String>,
) -> Result<Json<Workspace>, AppError> {
    account.require_scope(crate::protocol::scopes::WORKSPACES_READ)?;
    let id = uuid::Uuid::parse_str(&ws).map_err(|_| AppError::not_found())?;
    let row: Option<WorkspaceRow> = sqlx::query_as(&format!("{SELECT_FOR_MEMBER} WHERE w.id = $2"))
        .bind(account.user_id)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    to_dto(row.ok_or_else(AppError::not_found)?).map(Json)
}

/// `DELETE /v1/workspaces/{ws}`: scheduled deletion with 30-day export window
/// and 35-day backup retention. Returns `202 Accepted`.
pub async fn delete_workspace(
    State(state): State<AppState>,
    account: crate::auth::account::Account,
    Path(ws_str): Path<String>,
) -> Result<(StatusCode, Json<crate::protocol::DeleteWorkspaceResponse>), AppError> {
    account.require_scope(crate::protocol::scopes::WORKSPACES_WRITE)?;
    let id = uuid::Uuid::parse_str(&ws_str).map_err(|_| AppError::not_found())?;

    let mut tx = state.db.begin().await?;

    let role_row: Option<(String,)> = sqlx::query_as(
        "SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(account.user_id)
    .fetch_optional(&mut *tx)
    .await?;

    let (role,) = role_row.ok_or_else(AppError::not_found)?;
    if role != "owner" {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            crate::protocol::error_codes::FORBIDDEN_ROLE,
            "only owners can delete a workspace",
        ));
    }

    let now = OffsetDateTime::now_utc();
    let export_until = now + time::Duration::days(30);
    let primary_purge = now + time::Duration::days(30);
    let backups_expire = now + time::Duration::days(35);

    sqlx::query(
        "UPDATE workspaces
         SET deleted_at = $1, export_until = $2, primary_purge_at = $3, backups_expire_at = $4
         WHERE id = $5",
    )
    .bind(now)
    .bind(export_until)
    .bind(primary_purge)
    .bind(backups_expire)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    // Revoke transport tokens for devices
    sqlx::query("UPDATE devices SET status = 'revoked' WHERE workspace_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;

    // Cancel active subscriptions
    sqlx::query(
        "UPDATE subscriptions
         SET status = 'canceled', canceled_at = $1
         WHERE workspace_id = $2 AND status != 'canceled'",
    )
    .bind(now)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(crate::protocol::DeleteWorkspaceResponse {
            workspace_id: id.to_string(),
            status: "scheduled_for_deletion".into(),
            export_until: rfc3339(export_until)?,
            primary_purge_at: rfc3339(primary_purge)?,
            backups_expire_at: rfc3339(backups_expire)?,
        }),
    ))
}
