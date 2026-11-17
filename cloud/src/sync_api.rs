//! `GET/POST /v1/workspaces/{ws}/sync`: Git bundle transport with CAS.
//!
//! PostgreSQL holds the accepted head and the service's explicit trust
//! anchor for the workspace (`workspace_heads`). An upload's objects and its
//! immutable ref are stored before the transaction that may accept it; the
//! history is verified with `muster::fleet_state::verify_against` against
//! that anchor only (no local pin, home directory or private key).

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use muster::fleet_state::{signer_sets, FleetState, TrustAnchor, Verification, VerifyOptions};
use muster::manifest::Manifest;

use crate::auth::device_sig::{authenticate_device_request, AuthenticatedDevice};
use crate::error::AppError;
use crate::protocol::{
    error_codes, HeadRef, ProposalKind, ProposalStatus, SyncAccepted, SyncHeld, BASE_REVISION_NONE,
    GIT_BUNDLE_MEDIA_TYPE, HEADER_BASE_REVISION, HEADER_EPOCH, HEADER_REVISION,
};
use crate::repo_store::Staged;
use crate::state::AppState;

/// Maximum sync body (25 MB).
/// Request body cap for `POST /sync`: the largest per-push limit any plan
/// declares (each plan's own limit is enforced in the handler).
pub const MAX_BUNDLE_BYTES: usize = crate::billing::MAX_PLAN_BUNDLE_BYTES;

pub(crate) fn invalid(reason: &str, message: impl Into<String>) -> AppError {
    AppError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        error_codes::INVALID_HISTORY,
        message,
    )
    .with_details(serde_json::json!({ "reason": reason }))
}

pub(crate) fn not_approved(message: &str) -> AppError {
    AppError::new(
        StatusCode::FORBIDDEN,
        error_codes::DEVICE_NOT_APPROVED,
        message,
    )
}

/// Accepted head as recorded in PostgreSQL.
pub(crate) struct Head {
    pub revision: String,
    pub epoch: i64,
    pub anchor: TrustAnchor,
}

pub(crate) async fn load_head(
    conn: &mut sqlx::PgConnection,
    ws: uuid::Uuid,
) -> Result<Option<Head>, AppError> {
    let row: Option<(String, i64, serde_json::Value)> = sqlx::query_as(
        "SELECT head_revision, epoch, anchor FROM workspace_heads WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_optional(conn)
    .await?;
    row.map(|(revision, epoch, anchor)| {
        Ok(Head {
            revision,
            epoch,
            anchor: serde_json::from_value(anchor).map_err(AppError::internal)?,
        })
    })
    .transpose()
}

/// What the trust verifier concluded about an upload.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Verdict {
    Accepted {
        epoch: u64,
        signer: String,
        anchor: TrustAnchor,
        manifest: Manifest,
    },
    Pending {
        epoch: u64,
    },
}

/// Moves a staged upload the service keeps into the workspace repository
/// (objects, then its immutable ref), before the PostgreSQL commit.
async fn keep(state: &AppState, ws: uuid::Uuid, staged: Staged) -> Result<(), AppError> {
    let repos = state.repos.clone();
    tokio::task::spawn_blocking(move || repos.commit_staged(ws, staged))
        .await
        .map_err(AppError::internal)?
}

/// Verifies the complete history of a staged upload against `anchor`;
/// hands the upload back with the verdict (an error drops it).
pub(crate) async fn verify_upload(
    state: &AppState,
    staged: Staged,
    anchor: Option<TrustAnchor>,
) -> Result<(Staged, Verdict), AppError> {
    let repos = state.repos.clone();
    tokio::task::spawn_blocking(move || {
        let checkout = repos.checkout(&staged)?;
        let dir = checkout.path();
        let Some(fleet) =
            FleetState::load(dir).map_err(|e| invalid("unreadable_state", e.to_string()))?
        else {
            return Err(invalid(
                "legacy_unsigned",
                "the history has no signed fleet state; legacy repositories are never accepted",
            ));
        };
        let manifest = Manifest::load(dir).map_err(|e| invalid("bad_manifest", e.to_string()))?;
        let options = VerifyOptions {
            expect_signer: None,
            expect_state: None,
            persist: false,
            check_content: true,
        };
        let (verification, adopt) =
            muster::fleet_state::verify_against(dir, &manifest, anchor.as_ref(), &options)
                .map_err(|e| invalid("untrusted_history", e.to_string()))?;
        match verification {
            Verification::Verified { epoch, signer, .. } => Ok(Verdict::Accepted {
                epoch,
                signer,
                anchor: adopt.ok_or_else(|| AppError::internal("verified without anchor"))?,
                manifest,
            }),
            Verification::Pending { epoch, .. } => Ok(Verdict::Pending { epoch }),
            Verification::Legacy => Err(invalid(
                "legacy_unsigned",
                "the history has no signed fleet state; legacy repositories are never accepted",
            )),
        }
        .inspect(|_| {
            let _ = &fleet;
        })
        .map(|verdict| (staged, verdict))
    })
    .await
    .map_err(AppError::internal)?
}

fn authorized(anchor: &TrustAnchor, key: &str) -> bool {
    anchor.writers.iter().any(|k| k == key) || anchor.admins.iter().any(|k| k == key)
}

fn head_headers(response: &mut Response, revision: &str, epoch: u64) {
    if let Ok(v) = HeaderValue::from_str(revision) {
        response.headers_mut().insert(HEADER_REVISION, v);
    }
    response
        .headers_mut()
        .insert(HEADER_EPOCH, HeaderValue::from(epoch));
}

async fn device(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    ws: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<AuthenticatedDevice, AppError> {
    authenticate_device_request(state, method, uri.path(), uri.query(), ws, headers, body).await
}

/// Records the outcome of a push under its `Idempotency-Key`, in the
/// transaction that makes it durable: `200` accepted, `202` held and `409`
/// conflict (each names what the service kept). A retry with the same key
/// and bytes replays it instead of retaining the upload again.
#[allow(clippy::too_many_arguments)]
async fn remember(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    device: &AuthenticatedDevice,
    idem_key: uuid::Uuid,
    path: &str,
    base: &str,
    body_hash: &[u8],
    status: StatusCode,
    response: &serde_json::Value,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO sync_idempotency (workspace_id, device_id, idem_key, method, path, base_revision, body_sha256, status, headers, response)
         VALUES ($1, $2, $3, 'POST', $4, $5, $6, $7, '{}', $8)",
    )
    .bind(device.workspace_id)
    .bind(device.id)
    .bind(idem_key)
    .bind(path)
    .bind(base)
    .bind(body_hash)
    .bind(i32::from(status.as_u16()))
    .bind(response)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// `GET /v1/workspaces/{ws}/sync[?since=<rev>]`.
pub async fn get_sync(
    State(state): State<AppState>,
    Path(ws): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let device = device(&state, &method, &uri, &ws, &headers, &[]).await?;
    // Shared hold on the Git storage, before any row lock: a purge drains it.
    let _access = state.repo_access(device.workspace_id).await?;
    let mut conn = state.db.acquire().await?;
    let Some(head) = load_head(&mut conn, device.workspace_id).await? else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let since = uri.query().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "since")
            .map(|(_, v)| v.into_owned())
    });
    if since.as_deref() == Some(head.revision.as_str()) {
        let mut response = StatusCode::NO_CONTENT.into_response();
        head_headers(&mut response, &head.revision, head.epoch as u64);
        return Ok(response);
    }
    let repos = state.repos.clone();
    let ws_id = device.workspace_id;
    let revision = head.revision.clone();
    let bytes =
        tokio::task::spawn_blocking(move || repos.bundle(ws_id, &revision, since.as_deref()))
            .await
            .map_err(AppError::internal)??;
    let mut response = (StatusCode::OK, bytes).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(GIT_BUNDLE_MEDIA_TYPE),
    );
    head_headers(&mut response, &head.revision, head.epoch as u64);
    Ok(response)
}

/// `POST /v1/workspaces/{ws}/sync`.
pub async fn post_sync(
    State(state): State<AppState>,
    Path(ws): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    crate::tenancy::idempotency_key(&headers)?;
    let base = headers
        .get(HEADER_BASE_REVISION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            AppError::bad_request("X-Muster-Base-Revision is required (`none` when empty)")
                .with_details(serde_json::json!({ "header": "X-Muster-Base-Revision" }))
        })?
        .to_string();
    let device = device(&state, &method, &uri, &ws, &headers, &body).await?;
    let ws_id = device.workspace_id;
    // Shared hold on the Git storage, before any row lock: a purge drains it.
    // Held until this push's transaction ends.
    let _access = state.repo_access(ws_id).await?;

    let mut tx = state.db.begin().await?;
    // Serialise mutations carrying the same key for this device.
    let idem_key = crate::tenancy::idempotency_key(&headers)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text), hashtext($2::text))")
        .bind(device.id.to_string())
        .bind(idem_key.to_string())
        .execute(&mut *tx)
        .await?;
    // One publication at a time per workspace, and none (not even a stored
    // replay) once a deletion request is acknowledged: the lock and the
    // check come BEFORE the idempotency cache.
    crate::lifecycle::lock_writable(&mut tx, ws_id, true).await?;

    let body_hash = crate::auth::sha256(&body);
    let path = uri.path().to_string();
    let cached: Option<(String, String, Vec<u8>, i32, serde_json::Value)> = sqlx::query_as(
        "SELECT base_revision, path, body_sha256, status, response FROM sync_idempotency
         WHERE workspace_id = $1 AND device_id = $2 AND idem_key = $3",
    )
    .bind(ws_id)
    .bind(device.id)
    .bind(idem_key)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((prev_base, prev_path, prev_hash, status_code, resp_val)) = cached {
        if prev_path != path || prev_base != base || prev_hash != body_hash {
            return Err(AppError::idempotency_mismatch());
        }
        if status_code == i32::from(StatusCode::CONFLICT.as_u16()) {
            // A stored conflict replays as the same error: same proposal.
            let message = resp_val["message"].as_str().unwrap_or_default().to_string();
            return Err(AppError::new(
                StatusCode::CONFLICT,
                error_codes::REVISION_CONFLICT,
                message,
            )
            .with_details(resp_val["details"].clone()));
        }
        let status = StatusCode::from_u16(status_code as u16).map_err(AppError::internal)?;
        let mut resp = (status, Json(resp_val)).into_response();
        if let Ok(Some(h)) = load_head(&mut tx, ws_id).await {
            head_headers(&mut resp, &h.revision, h.epoch as u64);
        }
        return Ok(resp);
    }

    // Check plan entitlements (paid subscriptions or active trial).
    let sub = crate::billing::current_subscription(&mut *tx, ws_id).await?;

    let limits = crate::billing::limits_for(&state.config, sub.as_ref());
    let can_push = match sub {
        Some((_, status, ..)) => crate::billing::subscription_entitlement(&status).1,
        None => {
            let row: Option<(time::OffsetDateTime,)> =
                sqlx::query_as("SELECT trial_ends_at FROM workspaces WHERE id = $1")
                    .bind(ws_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            match row {
                Some((trial_end,)) => time::OffsetDateTime::now_utc() < trial_end,
                None => false,
            }
        }
    };

    if !can_push {
        return Err(AppError::new(
            StatusCode::PAYMENT_REQUIRED,
            error_codes::PLAN_INACTIVE,
            "workspace plan is inactive or expired; renew subscription to publish changes",
        ));
    }

    if body.len() > limits.max_bundle_bytes {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            error_codes::PAYLOAD_TOO_LARGE,
            "the bundle is larger than the workspace plan allows per push",
        )
        .with_details(serde_json::json!({
            "limit": "max_bundle_bytes",
            "max_bundle_bytes": limits.max_bundle_bytes,
        })));
    }

    let head = load_head(&mut tx, ws_id).await?;
    // Authorization before anything is staged or kept: a linked machine the
    // accepted governance does not authorize (or, before the first state,
    // one without the bootstrap claim) cannot make the service retain a
    // conflict or a held proposal either.
    let allowed = match &head {
        Some(h) => authorized(&h.anchor, &device.signing_key),
        None => device.bootstrap,
    };
    if !allowed {
        return Err(not_approved(if head.is_some() {
            "the accepted governance does not authorize this machine"
        } else {
            "only the machine holding the bootstrap claim may publish the first state"
        }));
    }

    // The upload is unpacked in private staging first: a push refused from
    // here on (quota, invalid history) leaves nothing in the workspace
    // repository.
    let upload = uuid::Uuid::new_v4();
    let repos = state.repos.clone();
    let bundle = body.clone();
    let (staged, object_bytes) = tokio::task::spawn_blocking(move || {
        let staged = repos.stage(ws_id, &bundle, upload)?;
        let bytes = repos.added_bytes(ws_id, &staged, false)?;
        Ok::<_, AppError>((staged, bytes))
    })
    .await
    .map_err(AppError::internal)??;
    let tip = staged.tip.clone();

    // Storage counts the bytes keeping this upload really adds (object
    // files plus refs), retained conflicts and proposals included. The
    // workspace lock serialises this check with the move below.
    let added = object_bytes + u64::from(head.is_none()) * crate::repo_store::REF_FILE_BYTES;
    crate::billing::ensure_storage_fits(&state, &mut tx, ws_id, added).await?;

    let anchor = head.as_ref().map(|h| h.anchor.clone());
    let conflict = match &head {
        None if base != BASE_REVISION_NONE => Some(BASE_REVISION_NONE.to_string()),
        None => None,
        Some(h) if base != h.revision => Some(h.revision.clone()),
        Some(_) => None,
    };
    if let Some(accepted_head) = conflict {
        let proposal_id = upload;
        keep(&state, ws_id, staged).await?;
        sqlx::query(
            "INSERT INTO proposals (id, workspace_id, device_id, kind, status, base_revision, revision, epoch)
             VALUES ($1, $2, $3, 'conflict', 'open', $4, $5, NULL)",
        )
        .bind(proposal_id)
        .bind(ws_id)
        .bind(device.id)
        .bind(&base)
        .bind(&tip)
        .execute(&mut *tx)
        .await?;
        let epoch = head.as_ref().map(|h| h.epoch as u64).unwrap_or(0);
        let details = crate::protocol::ConflictDetails {
            head: accepted_head,
            epoch,
            proposal_id: proposal_id.to_string(),
        };
        let err = AppError::new(
            StatusCode::CONFLICT,
            error_codes::REVISION_CONFLICT,
            "base revision does not match the accepted head",
        )
        .with_details(serde_json::to_value(details).map_err(AppError::internal)?);
        let stored = serde_json::json!({
            "code": err.code,
            "message": err.message,
            "details": err.details,
        });
        remember(
            &mut tx,
            &device,
            idem_key,
            &path,
            &base,
            &body_hash,
            StatusCode::CONFLICT,
            &stored,
        )
        .await?;
        tx.commit().await?;
        return Err(err);
    }

    let (staged, verdict) = verify_upload(&state, staged, anchor).await?;
    let (epoch, signer, new_anchor, manifest) = match verdict {
        Verdict::Accepted {
            epoch,
            signer,
            anchor,
            manifest,
        } => (epoch, signer, anchor, manifest),
        Verdict::Pending { epoch } => {
            // Same id as the upload: its anti-GC ref keeps the proposal.
            let proposal_id = upload;
            keep(&state, ws_id, staged).await?;
            sqlx::query(
                "INSERT INTO proposals (id, workspace_id, device_id, kind, status, base_revision, revision, epoch)
                 VALUES ($1, $2, $3, 'pending_quorum', 'open', $4, $5, $6)",
            )
            .bind(proposal_id)
            .bind(ws_id)
            .bind(device.id)
            .bind(&base)
            .bind(&tip)
            .bind(epoch as i64)
            .execute(&mut *tx)
            .await?;
            let held = SyncHeld {
                proposal_id: proposal_id.to_string(),
                status: ProposalStatus::Open,
                kind: ProposalKind::PendingQuorum,
                head: head.as_ref().map(|h| HeadRef {
                    revision: h.revision.clone(),
                    epoch: h.epoch as u64,
                }),
            };
            let stored = serde_json::to_value(&held).map_err(AppError::internal)?;
            remember(
                &mut tx,
                &device,
                idem_key,
                &path,
                &base,
                &body_hash,
                StatusCode::ACCEPTED,
                &stored,
            )
            .await?;
            tx.commit().await?;
            return Ok((StatusCode::ACCEPTED, Json(held)).into_response());
        }
    };
    if head.is_none() {
        let (admins, writers) = signer_sets(&manifest);
        if !admins.contains(&device.signing_key) && !writers.contains(&device.signing_key) {
            return Err(not_approved(
                "the first state's governance does not authorize this machine's key",
            ));
        }
    }

    keep(&state, ws_id, staged).await?;
    let anchor_json = serde_json::to_value(&new_anchor).map_err(AppError::internal)?;
    sqlx::query(
        "INSERT INTO workspace_heads (workspace_id, head_revision, epoch, payload_hash, anchor)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (workspace_id) DO UPDATE SET head_revision = EXCLUDED.head_revision,
           epoch = EXCLUDED.epoch, payload_hash = EXCLUDED.payload_hash,
           anchor = EXCLUDED.anchor, updated_at = now()",
    )
    .bind(ws_id)
    .bind(&tip)
    .bind(epoch as i64)
    .bind(&new_anchor.payload_hash)
    .bind(&anchor_json)
    .execute(&mut *tx)
    .await?;
    let repos = state.repos.clone();
    let rev_for_subject = tip.clone();
    let message = tokio::task::spawn_blocking(move || repos.subject(ws_id, &rev_for_subject))
        .await
        .map_err(AppError::internal)??;
    sqlx::query(
        "INSERT INTO accepted_revisions (workspace_id, revision, parent_revision, epoch, signer, device_id, message)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (workspace_id, revision) DO NOTHING",
    )
    .bind(ws_id)
    .bind(&tip)
    .bind(head.as_ref().map(|h| h.revision.clone()))
    .bind(epoch as i64)
    .bind(&signer)
    .bind(device.id)
    .bind(&message)
    .execute(&mut *tx)
    .await?;
    let accepted_dto = SyncAccepted {
        revision: tip.clone(),
        epoch,
    };
    let accepted_val = serde_json::to_value(&accepted_dto).map_err(AppError::internal)?;
    remember(
        &mut tx,
        &device,
        idem_key,
        &path,
        &base,
        &body_hash,
        StatusCode::OK,
        &accepted_val,
    )
    .await?;
    tx.commit().await?;

    // Derived ref; repaired again before every read, so a failure here is
    // not fatal to what PostgreSQL accepted.
    let repos = state.repos.clone();
    let rev = tip.clone();
    if let Err(err) = tokio::task::spawn_blocking(move || repos.set_main(ws_id, &rev))
        .await
        .map_err(AppError::internal)?
    {
        tracing::warn!(error = %err.message, "main not updated; repaired on next read");
    }
    let mut response = (StatusCode::OK, Json(accepted_dto)).into_response();
    head_headers(&mut response, &tip, epoch);
    Ok(response)
}
