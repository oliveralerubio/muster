//! Automatic upkeep, run by a background worker on the active instance only
//! (a fenced one does nothing):
//!
//! - closed proposals give their retained uploads back, so storage use (and
//!   a `507 quota_exceeded`) recovers without an operator step;
//! - bookkeeping that only grows is deleted past its retention: replay
//!   nonces once no request carrying them can still be accepted
//!   ([`NONCE_RETENTION`]), idempotency records after
//!   [`IDEMPOTENCY_RETENTION`], and expired account tokens, sessions,
//!   OIDC logins and device authorizations.

use std::time::Duration;

use uuid::Uuid;

use crate::state::AppState;

/// Workspaces released per pass (the next pass continues).
const RELEASE_BATCH: i64 = 20;

/// How long a replay nonce is kept after its request was accepted. A
/// request signed at `t` is accepted while the server clock is within
/// `t ± SIGNED_REQUEST_MAX_SKEW_SECS`, so its nonce (stored at some such
/// moment) can be replayed for at most twice the skew after it was stored;
/// this keeps it longer than that, with a margin.
pub const NONCE_RETENTION: Duration =
    Duration::from_secs(2 * crate::protocol::SIGNED_REQUEST_MAX_SKEW_SECS as u64 + 300);
const _: () =
    assert!(NONCE_RETENTION.as_secs() > 2 * crate::protocol::SIGNED_REQUEST_MAX_SKEW_SECS as u64);

/// How long an idempotency record (account requests and pushes) is kept:
/// far longer than any client keeps retrying one request (a machine that
/// was offline retries its pending push with the same key when it returns).
pub const IDEMPOTENCY_RETENTION: Duration = Duration::from_secs(30 * 24 * 3600);

/// Expired device authorizations are kept this long for diagnostics.
const DEVICE_AUTHORIZATION_GRACE: Duration = Duration::from_secs(24 * 3600);

/// Rows deleted per statement, and statements per table per pass.
const DELETE_BATCH: i64 = 5000;
const DELETE_ROUNDS: usize = 20;

/// Runs [`run_once`] every `every`, forever.
pub async fn worker(state: AppState, every: Duration) {
    loop {
        if state
            .role_gate
            .allows(&state.config.control, &state.db, "maintenance")
            .await
        {
            run_once(&state).await;
        }
        tokio::time::sleep(every).await;
    }
}

/// One maintenance pass. Failures are logged and retried by the next pass.
pub async fn run_once(state: &AppState) {
    if let Err(err) = release_closed_uploads(state).await {
        tracing::warn!(error = %err, "maintenance: closed proposal release will be retried");
    }
    if let Err(err) = prune(state).await {
        tracing::warn!(error = %err, "maintenance: cleanup will be retried");
    }
}

/// Deletes bookkeeping past its retention, in batches.
async fn prune(state: &AppState) -> Result<(), String> {
    let idempotency = state
        .config
        .dev_idempotency_retention_secs
        .map(Duration::from_secs)
        .unwrap_or(IDEMPOTENCY_RETENTION);
    let tables: [(&str, &str, Duration); 7] = [
        ("device_nonces", "created_at", NONCE_RETENTION),
        ("sync_idempotency", "created_at", idempotency),
        ("account_idempotency", "created_at", idempotency),
        ("account_tokens", "expires_at", Duration::ZERO),
        ("sessions", "expires_at", Duration::ZERO),
        ("oidc_logins", "expires_at", Duration::ZERO),
        (
            "device_authorizations",
            "expires_at",
            DEVICE_AUTHORIZATION_GRACE,
        ),
    ];
    for (table, column, keep) in tables {
        // Fixed identifiers from the list above; only the age is bound.
        let sql = format!(
            "DELETE FROM {table} WHERE ctid IN (
               SELECT ctid FROM {table}
                WHERE {column} < now() - make_interval(secs => $1)
                LIMIT $2)"
        );
        let mut removed = 0u64;
        for _ in 0..DELETE_ROUNDS {
            let done = sqlx::query(&sql)
                .bind(keep.as_secs_f64())
                .bind(DELETE_BATCH)
                .execute(&state.db)
                .await
                .map_err(|e| format!("{table}: {e}"))?
                .rows_affected();
            removed += done;
            if done < DELETE_BATCH as u64 {
                break;
            }
        }
        if removed > 0 {
            tracing::info!(table, removed, "maintenance: expired rows deleted");
        }
    }
    Ok(())
}

/// Releases the retained uploads of closed proposals, workspace by
/// workspace.
async fn release_closed_uploads(state: &AppState) -> Result<(), String> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT workspace_id FROM proposals
          WHERE has_bundle AND bundle_released_at IS NULL AND status <> 'open'
          LIMIT $1",
    )
    .bind(RELEASE_BATCH)
    .fetch_all(&state.db)
    .await
    .map_err(|e| format!("database: {e}"))?;
    for ws in due {
        release_workspace(state, ws).await?;
    }
    Ok(())
}

async fn release_workspace(state: &AppState, ws: Uuid) -> Result<(), String> {
    // A backup capture lists objects live: never repack under one. Shared,
    // like a purge; a capture in progress makes this pass skip (retried).
    let Some(_capture) =
        crate::lifecycle_purge::capture_lock(&state.config.repo_root, false, Duration::ZERO)
            .await
            .map_err(|e| format!("backup coordination lock: {e}"))?
    else {
        return Ok(());
    };
    // Shared hold on the Git storage before any row lock, like a request:
    // a purge drains it; a tombstoned workspace is left to the purge.
    let Ok(_access) = state.repo_access(ws).await else {
        return Ok(());
    };
    let mut tx = state.db.begin().await.map_err(|e| e.to_string())?;
    // The workspace row lock: no push stages against, or keeps into, this
    // repository while objects are being dropped.
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(ws)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    if found.is_none() {
        return Ok(());
    }
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM proposals
          WHERE workspace_id = $1 AND has_bundle AND bundle_released_at IS NULL
            AND status <> 'open'
          FOR UPDATE",
    )
    .bind(ws)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    if ids.is_empty() {
        return Ok(());
    }
    let repos = state.repos.clone();
    let released = ids.clone();
    tokio::task::spawn_blocking(move || repos.release_uploads(ws, &released))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.message)?;
    sqlx::query("UPDATE proposals SET bundle_released_at = now() WHERE id = ANY($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    tracing::info!(workspace = %ws, proposals = ids.len(), "maintenance: closed proposal uploads released");
    Ok(())
}
