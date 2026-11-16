//! Workspace lifecycle: the owner-confirmed deletion request and the state
//! every member can read.
//!
//! A deletion request is ONE idempotent operation per workspace. It is
//! journaled in the lifecycle authority (`crate::control`, outside the
//! database and its backups) and projected into the same transaction that
//! holds the workspace row lock, before `202` is sent. Its deadlines are
//! fixed when it is first made and never recomputed: a retry, or a new
//! request with another key, returns the same request. Nothing is purged
//! here; `due` means the primary purge may run, not that it ran.
//!
//! Writers that a request must stop take the same workspace row lock and
//! call [`deny_if_scheduled`] inside their transaction, before any stored
//! idempotent replay, so no write is accepted or replayed once the request
//! is acknowledged. Pull and export stay available during the grace.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use sqlx::{Postgres, Transaction};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::control::{self, Record};
use crate::error::AppError;
use crate::protocol::{
    error_codes, scopes, BackupPurge, DeleteWorkspaceRequest, DeletionStatus, LifecycleState,
    PrimaryPurge, ProviderCancellation, ProviderCancellationItem, WorkspaceLifecycle,
};
use crate::state::AppState;
use crate::tenancy::{idempotency_key, parse_json};

/// Export window and primary-data grace after a request.
pub const PRIMARY_GRACE: time::Duration = time::Duration::days(30);
/// Policy cap for backup copies after a request.
pub const BACKUP_CAP: time::Duration = time::Duration::days(35);

const BACKUP_UNKNOWN_REASON: &str =
    "no backup catalog yet: the cap applies to catalogued snapshots, none are tracked";

fn rfc3339(t: OffsetDateTime) -> Result<String, AppError> {
    t.format(&Rfc3339).map_err(AppError::internal)
}

/// Grace and cap for a new request (shortened only by the development
/// setting `MUSTER_DEV_DELETION_GRACE_SECS`, keeping the 30:35 ratio).
fn deadlines(state: &AppState) -> (time::Duration, time::Duration) {
    match state.config.dev_deletion_grace_secs {
        Some(secs) => {
            let grace = time::Duration::seconds(secs as i64);
            (grace, grace * 35 / 30)
        }
        None => (PRIMARY_GRACE, BACKUP_CAP),
    }
}

fn authority_error(err: control::ControlError) -> AppError {
    tracing::error!(error = %err, "lifecycle authority unavailable for a deletion request");
    AppError::not_ready("lifecycle authority unavailable")
}

fn us(t: OffsetDateTime) -> i64 {
    (t.unix_timestamp_nanos() / 1000) as i64
}

/// `409 workspace_deletion_scheduled` when `ws` has a deletion request.
/// Call inside the writer's transaction, after taking the workspace row
/// lock (so it serialises with the request) and before any stored replay.
pub async fn deny_if_scheduled(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
) -> Result<(), AppError> {
    let row: Option<(Uuid, OffsetDateTime, bool)> = sqlx::query_as(
        "SELECT request_id, primary_purge_at, purge_committed_at IS NOT NULL
           FROM workspace_lifecycle WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_optional(&mut **tx)
    .await?;
    match row {
        None => Ok(()),
        // Tombstoned: the workspace no longer exists for anyone.
        Some((_, _, true)) => Err(AppError::not_found()),
        Some((request_id, purge_at, false)) => Err(AppError::new(
            StatusCode::CONFLICT,
            error_codes::WORKSPACE_DELETION_SCHEDULED,
            "this workspace is scheduled for deletion; only pull and export remain available",
        )
        .with_details(serde_json::json!({
            "request_id": request_id.to_string(),
            "primary_purge_at": rfc3339(purge_at)?,
        }))),
    }
}

/// Takes the workspace row lock in `tx` (`FOR UPDATE` for writers that
/// serialise with each other, `FOR SHARE` otherwise; both conflict with the
/// deletion request's `FOR UPDATE`) and then refuses with
/// [`deny_if_scheduled`].
pub async fn lock_writable(
    tx: &mut Transaction<'_, Postgres>,
    ws: Uuid,
    exclusive: bool,
) -> Result<(), AppError> {
    let query = if exclusive {
        "SELECT id FROM workspaces WHERE id = $1 FOR UPDATE"
    } else {
        "SELECT id FROM workspaces WHERE id = $1 FOR SHARE"
    };
    let found = sqlx::query(query)
        .bind(ws)
        .fetch_optional(&mut **tx)
        .await?;
    if found.is_none() {
        return Err(AppError::not_found());
    }
    deny_if_scheduled(tx, ws).await
}

/// `(item, provider_id, status, confirmed_at, last_error)`.
type ItemRow = (
    String,
    String,
    String,
    Option<OffsetDateTime>,
    Option<String>,
);

/// `(request_id, requested_at, primary_purge_at, backup_cap_at, now,
/// purge_committed_at, purged_at, purge_last_error)`.
type LifecycleRow = (
    Uuid,
    OffsetDateTime,
    OffsetDateTime,
    OffsetDateTime,
    OffsetDateTime,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
    Option<String>,
);

/// The lifecycle of `ws` as the API reports it.
pub async fn view<'e, E>(db: E, ws: Uuid) -> Result<WorkspaceLifecycle, AppError>
where
    E: sqlx::PgExecutor<'e> + Copy,
{
    let row: Option<LifecycleRow> = sqlx::query_as(
        "SELECT request_id, requested_at, primary_purge_at, backup_cap_at, now(),
                purge_committed_at, purged_at, purge_last_error
               FROM workspace_lifecycle WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_optional(db)
    .await?;
    let Some((
        request_id,
        requested_at,
        purge_at,
        cap_at,
        now,
        committed_at,
        purged_at,
        last_error,
    )) = row
    else {
        return Ok(WorkspaceLifecycle {
            workspace_id: ws.to_string(),
            state: LifecycleState::Active,
            deletion: None,
        });
    };
    let items: Vec<ItemRow> = sqlx::query_as(
        "SELECT item, provider_id, status, confirmed_at, last_error
               FROM lifecycle_provider_items WHERE workspace_id = $1
              ORDER BY created_at, item, provider_id",
    )
    .bind(ws)
    .fetch_all(db)
    .await?;
    let provider_status = if items.is_empty() {
        "none"
    } else if items.iter().all(|i| i.2 == "confirmed") {
        "confirmed"
    } else {
        "pending"
    };
    let items = items
        .into_iter()
        .map(|(kind, id, status, confirmed_at, last_error)| {
            Ok(ProviderCancellationItem {
                kind,
                id,
                status,
                confirmed_at: confirmed_at.map(rfc3339).transpose()?,
                last_error,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    Ok(WorkspaceLifecycle {
        workspace_id: ws.to_string(),
        state: LifecycleState::DeletionScheduled,
        deletion: Some(DeletionStatus {
            request_id: request_id.to_string(),
            requested_at: rfc3339(requested_at)?,
            export_until: rfc3339(purge_at)?,
            primary: PrimaryPurge {
                // `due` only reads the clock; only the journal says more.
                status: match (committed_at, purged_at) {
                    (_, Some(_)) => "purged",
                    (Some(_), None) => "purge_committed",
                    _ if now < purge_at => "scheduled",
                    _ => "due",
                }
                .into(),
                purge_at: rfc3339(purge_at)?,
                purge_committed_at: committed_at.map(rfc3339).transpose()?,
                purged_at: purged_at.map(rfc3339).transpose()?,
                // Only while unfinished: an operational reason, no data.
                last_error: if purged_at.is_none() {
                    last_error
                } else {
                    None
                },
            },
            backups: BackupPurge {
                status: "unknown".into(),
                deadline: None,
                cap_at: rfc3339(cap_at)?,
                purged_at: None,
                reason: Some(BACKUP_UNKNOWN_REASON.into()),
            },
            provider: ProviderCancellation {
                status: provider_status.into(),
                items,
            },
            writes_allowed: false,
            pull_allowed: committed_at.is_none(),
            export_allowed: committed_at.is_none(),
        }),
    })
}

/// `GET /v1/workspaces/{ws}/lifecycle`: any member, and the account that
/// requested the deletion (kept outside the tenant rows, so it can follow
/// the purge to the end after its membership is gone).
pub async fn get_lifecycle(
    State(state): State<AppState>,
    account: crate::auth::account::Account,
    Path(ws): Path<String>,
) -> Result<Json<WorkspaceLifecycle>, AppError> {
    account.require_scope(scopes::WORKSPACES_READ)?;
    let id = Uuid::parse_str(&ws).map_err(|_| AppError::not_found())?;
    let member: Option<(String,)> =
        sqlx::query_as("SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2")
            .bind(id)
            .bind(account.user_id)
            .fetch_optional(&state.db)
            .await?;
    if member.is_none() {
        let requester: Option<(Uuid,)> = sqlx::query_as(
            "SELECT request_id FROM workspace_lifecycle WHERE workspace_id = $1 AND requested_by = $2",
        )
        .bind(id)
        .bind(account.user_id)
        .fetch_optional(&state.db)
        .await?;
        requester.ok_or_else(AppError::not_found)?;
    }
    view(&state.db, id).await.map(Json)
}

/// `DELETE /v1/workspaces/{ws}`: owner-confirmed deletion request.
pub async fn request_deletion(
    State(state): State<AppState>,
    account: crate::auth::account::Account,
    Path(ws): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<WorkspaceLifecycle>), AppError> {
    account.require_scope(scopes::WORKSPACES_WRITE)?;
    let id = Uuid::parse_str(&ws).map_err(|_| AppError::not_found())?;

    let mut tx = state.db.begin().await?;
    // The workspace row lock serialises the request with every writer it
    // stops (sync, links, proposals, invitations, checkout, plan changes).
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT w.name, m.role FROM workspaces w
           JOIN memberships m ON m.workspace_id = w.id AND m.user_id = $2
          WHERE w.id = $1 FOR UPDATE OF w",
    )
    .bind(id)
    .bind(account.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (name, role) = row.ok_or_else(AppError::not_found)?;
    if role != "owner" {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            error_codes::FORBIDDEN_ROLE,
            "only owners can delete a workspace",
        ));
    }
    let key = idempotency_key(&headers)?;
    let req: DeleteWorkspaceRequest = parse_json(&body)?;
    if req.confirm_workspace_id != id.to_string() || req.confirm_workspace_name != name {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            error_codes::CONFIRMATION_MISMATCH,
            "confirm_workspace_id and confirm_workspace_name must be exactly this workspace's id and name",
        ));
    }

    let body_hash = crate::auth::sha256(&body);
    let path = format!("/v1/workspaces/{id}");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text), hashtext($2::text))")
        .bind(account.user_id.to_string())
        .bind(key.to_string())
        .execute(&mut *tx)
        .await?;
    let previous: Option<(String, String, Vec<u8>)> = sqlx::query_as(
        "SELECT method, path, body_sha256 FROM account_idempotency
          WHERE user_id = $1 AND idem_key = $2",
    )
    .bind(account.user_id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((method, prev_path, prev_hash)) = previous {
        if method != "DELETE" || prev_path != path || prev_hash != body_hash {
            return Err(AppError::idempotency_mismatch());
        }
        // Replays report the live state of the one request.
        drop(tx);
        return Ok((StatusCode::ACCEPTED, Json(view(&state.db, id).await?)));
    }

    // Catch the projection up in this transaction (a request journaled
    // before a crash is projected here) and require the active incarnation.
    let mut authority = control::open_for_write(&state.config.control, &mut tx)
        .await
        .map_err(authority_error)?;
    let request_id = match authority.deletion_request(id) {
        Some(Record::WorkspaceDeletionRequested { request_id, .. }) => *request_id,
        _ => {
            let subscriptions: Vec<String> = sqlx::query_scalar(
                "SELECT paddle_subscription_id FROM subscriptions
                  WHERE workspace_id = $1 AND status <> 'canceled' ORDER BY paddle_subscription_id",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
            let checkouts: Vec<String> = sqlx::query_scalar(
                "SELECT transaction_id FROM billing_checkouts
                  WHERE workspace_id = $1 AND status = 'open' ORDER BY transaction_id",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
            // Microsecond precision: exactly what the database projection keeps.
            let now = OffsetDateTime::now_utc();
            let now = now
                .replace_nanosecond(now.nanosecond() / 1000 * 1000)
                .map_err(AppError::internal)?;
            let (grace, cap) = deadlines(&state);
            let request_id = Uuid::new_v4();
            authority
                .append_projected(
                    &mut tx,
                    Record::WorkspaceDeletionRequested {
                        workspace_id: id,
                        request_id,
                        requested_by: account.user_id,
                        requested_at_us: us(now),
                        primary_purge_at_us: us(now + grace),
                        backup_cap_at_us: us(now + cap),
                        subscriptions,
                        checkouts,
                    },
                    state.config.dev_lifecycle_crash,
                )
                .await
                .map_err(authority_error)?;
            request_id
        }
    };
    sqlx::query(
        "INSERT INTO account_idempotency (user_id, idem_key, method, path, body_sha256, status, response)
         VALUES ($1, $2, 'DELETE', $3, $4, 202, $5)",
    )
    .bind(account.user_id)
    .bind(key)
    .bind(&path)
    .bind(&body_hash)
    .bind(serde_json::json!({ "request_id": request_id.to_string() }))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    drop(authority);
    // One immediate attempt; the worker retries whatever stays pending.
    crate::lifecycle_provider::attempt_workspace(&state, id).await;
    Ok((StatusCode::ACCEPTED, Json(view(&state.db, id).await?)))
}
