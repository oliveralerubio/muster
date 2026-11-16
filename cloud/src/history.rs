//! History, diffs, and bundle export.
//!
//! Export is always available to members (and machines), regardless of plan
//! status (trial, expired, or active). The bundle is self-contained and
//! verified by the open-source CLI without network access.

use axum::extract::{Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::account::Account;
use crate::error::AppError;
use crate::protocol::{
    scopes, DiffFile, ExportManifest, RevisionDiff, RevisionItem, GIT_BUNDLE_MEDIA_TYPE,
    HEADER_EPOCH, HEADER_EXPORT_MANIFEST, HEADER_REVISION,
};
use crate::state::AppState;

fn rfc3339(t: OffsetDateTime) -> Result<String, AppError> {
    t.format(&Rfc3339).map_err(AppError::internal)
}

fn parse_uuid(s: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(s).map_err(|_| AppError::not_found())
}

async fn check_member(state: &AppState, ws: Uuid, user: Uuid) -> Result<(), AppError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2")
            .bind(ws)
            .bind(user)
            .fetch_optional(&state.db)
            .await?;
    if row.is_none() {
        return Err(AppError::not_found());
    }
    Ok(())
}

#[derive(serde::Deserialize, Default)]
pub struct RevisionsQuery {
    pub limit: Option<usize>,
    pub before: Option<String>,
}

/// `GET /v1/workspaces/{ws}/revisions`: accepted revisions, newest first.
pub async fn list_revisions(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
    Query(query): Query<RevisionsQuery>,
) -> Result<Json<Vec<RevisionItem>>, AppError> {
    account.require_scope(scopes::WORKSPACES_READ)?;
    let ws = parse_uuid(&ws_str)?;
    check_member(&state, ws, account.user_id).await?;

    let limit = query.limit.unwrap_or(50).clamp(1, 200) as i64;

    type RevisionRow = (
        String,
        Option<String>,
        i64,
        String,
        Uuid,
        String,
        OffsetDateTime,
    );
    let rows: Vec<RevisionRow> = if let Some(before_rev) = query.before {
        let before_seq: Option<(i64,)> = sqlx::query_as(
            "SELECT seq FROM accepted_revisions WHERE workspace_id = $1 AND revision = $2",
        )
        .bind(ws)
        .bind(&before_rev)
        .fetch_optional(&state.db)
        .await?;
        let Some((seq,)) = before_seq else {
            return Ok(Json(Vec::new()));
        };
        sqlx::query_as(
            "SELECT revision, parent_revision, epoch, signer, device_id, message, accepted_at
                 FROM accepted_revisions
                 WHERE workspace_id = $1 AND seq < $2
                 ORDER BY seq DESC
                 LIMIT $3",
        )
        .bind(ws)
        .bind(seq)
        .bind(limit)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as(
            "SELECT revision, parent_revision, epoch, signer, device_id, message, accepted_at
                 FROM accepted_revisions
                 WHERE workspace_id = $1
                 ORDER BY seq DESC
                 LIMIT $2",
        )
        .bind(ws)
        .bind(limit)
        .fetch_all(&state.db)
        .await?
    };

    let items = rows
        .into_iter()
        .map(|(rev, parent, ep, signer, dev_id, msg, at)| {
            Ok(RevisionItem {
                revision: rev,
                parent_revision: parent,
                epoch: ep as u64,
                signer,
                device_id: dev_id.to_string(),
                message: msg,
                accepted_at: rfc3339(at)?,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;

    Ok(Json(items))
}

/// `GET /v1/workspaces/{ws}/revisions/{rev}/diff`: per-file unified diff.
pub async fn diff_revision(
    State(state): State<AppState>,
    account: Account,
    Path((ws_str, rev)): Path<(String, String)>,
) -> Result<Json<RevisionDiff>, AppError> {
    account.require_scope(scopes::WORKSPACES_READ)?;
    let ws = parse_uuid(&ws_str)?;
    check_member(&state, ws, account.user_id).await?;
    let _access = state.repo_access(ws).await?;

    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT parent_revision FROM accepted_revisions WHERE workspace_id = $1 AND revision = $2",
    )
    .bind(ws)
    .bind(&rev)
    .fetch_optional(&state.db)
    .await?;

    let (parent_rev,) = row.ok_or_else(AppError::not_found)?;

    let repos = state.repos.clone();
    let r = rev.clone();
    let p = parent_rev.clone();
    let files: Vec<DiffFile> =
        tokio::task::spawn_blocking(move || repos.diff(ws, p.as_deref(), &r))
            .await
            .map_err(AppError::internal)??;

    Ok(Json(RevisionDiff {
        revision: rev,
        base_revision: parent_rev,
        files,
    }))
}

/// `GET /v1/workspaces/{ws}/export`: complete Git bundle at the accepted head.
pub async fn export_bundle(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
) -> Result<Response, AppError> {
    account.require_scope(scopes::WORKSPACES_READ)?;
    let ws = parse_uuid(&ws_str)?;
    check_member(&state, ws, account.user_id).await?;
    let _access = state.repo_access(ws).await?;

    type HeadRow = (String, i64, String);
    let head_row: Option<HeadRow> = sqlx::query_as(
        "SELECT r.revision, r.epoch, r.signer
         FROM accepted_revisions r
         WHERE r.workspace_id = $1
         ORDER BY r.seq DESC
         LIMIT 1",
    )
    .bind(ws)
    .fetch_optional(&state.db)
    .await?;

    let Some((head_rev, epoch, signer)) = head_row else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };

    let repos = state.repos.clone();
    let rev = head_rev.clone();
    let bundle_bytes = tokio::task::spawn_blocking(move || repos.bundle(ws, &rev, None))
        .await
        .map_err(AppError::internal)??;

    let sha = crate::protocol::body_sha256_hex(&bundle_bytes);
    let manifest = ExportManifest {
        revision: head_rev.clone(),
        epoch: epoch as u64,
        signer,
        sha256: sha,
    };
    let manifest_json = serde_json::to_string(&manifest).map_err(AppError::internal)?;

    let mut response = (StatusCode::OK, bundle_bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(GIT_BUNDLE_MEDIA_TYPE),
    );
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"workspace.bundle\""),
    );
    headers.insert(
        HEADER_REVISION,
        HeaderValue::from_str(&head_rev).map_err(AppError::internal)?,
    );
    headers.insert(
        HEADER_EPOCH,
        HeaderValue::from_str(&epoch.to_string()).map_err(AppError::internal)?,
    );
    headers.insert(
        HEADER_EXPORT_MANIFEST,
        HeaderValue::from_str(&manifest_json).map_err(AppError::internal)?,
    );

    Ok(response)
}
