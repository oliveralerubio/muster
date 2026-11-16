//! Immediate provider (Paddle) cancellation for deletion requests.
//!
//! Every provider object a deletion request must stop is a durable item
//! (`lifecycle_provider_items`, projected from the lifecycle journal, so a
//! restored older database still knows it). An item is `confirmed` ONLY
//! after a canonical read from Paddle shows it canceled:
//!
//! - subscription: `GET /subscriptions/{id}`; unless already `canceled`,
//!   `POST /subscriptions/{id}/cancel {"effective_from":"immediately"}`, then
//!   `GET` again, confirmed when `status == canceled`;
//! - checkout transaction: `PATCH /transactions/{id} {"status":"canceled"}`,
//!   then `GET /transactions/{id}`: `canceled` confirms it; `completed` or
//!   `billed` (paid in the meantime) journals the subscription Paddle created
//!   from it as a new item and confirms the checkout as resolved by payment.
//!
//! Network errors, 5xx, 404 and unexpected states never confirm anything:
//! the item stays `pending` with `last_error` and is retried with
//! exponential backoff (at most an hour), by the request itself, by the
//! background worker and after restarts. A subscription that turns live
//! after the request (late completion, reactivation) is registered from the
//! canonical reconciliation ([`register_live_subscription`]).

use std::time::Duration;

use uuid::Uuid;

use crate::billing_reconcile::paddle_http;
use crate::config::PaddleConfig;
use crate::control::{self, ProviderItem, Record};
use crate::error::AppError;
use crate::state::AppState;

const MAX_BACKOFF_SECS: i64 = 3600;
const LEASE_SECS: f64 = 60.0;
const UNCONFIGURED: &str = "billing provider is not configured on this server";

enum Outcome {
    /// Canonically canceled (or resolved by payment), with the status read.
    Confirmed(String),
    /// Not (yet) canceled: retry later.
    Retry(String),
}

type Claimed = (String, String, Uuid, Uuid, i32);

/// Attempts every pending item of `ws` now (after the request commits).
pub async fn attempt_workspace(state: &AppState, ws: Uuid) {
    let Some(paddle) = &state.config.paddle else {
        let _ = sqlx::query(
            "UPDATE lifecycle_provider_items SET last_error = $2
              WHERE workspace_id = $1 AND status = 'pending'",
        )
        .bind(ws)
        .bind(UNCONFIGURED)
        .execute(&state.db)
        .await;
        return;
    };
    // Items found while processing (a paid checkout's subscription) are
    // attempted in the same call.
    for _ in 0..3 {
        let claimed = claim(state, Some(ws)).await;
        if claimed.is_empty() {
            return;
        }
        for item in claimed {
            process(state, paddle, item).await;
        }
    }
}

/// Runs every due item once; returns how many were attempted.
pub async fn run_due(state: &AppState) -> usize {
    let Some(paddle) = &state.config.paddle else {
        return 0;
    };
    let claimed = claim(state, None).await;
    let n = claimed.len();
    for item in claimed {
        process(state, paddle, item).await;
    }
    n
}

/// Background loop: retries pending cancellations, including those left by
/// a previous process.
pub async fn worker(state: AppState, every: Duration) {
    loop {
        run_due(&state).await;
        tokio::time::sleep(every).await;
    }
}

/// Leases up to 20 due pending items (of `ws` only, when given). A failed
/// attempt pushes `next_attempt_at` out, so it is not retried in a loop.
async fn claim(state: &AppState, ws: Option<Uuid>) -> Vec<Claimed> {
    let rows = sqlx::query_as::<_, Claimed>(
        "UPDATE lifecycle_provider_items
            SET locked_until = now() + make_interval(secs => $2)
          WHERE (item, provider_id) IN (
                SELECT item, provider_id FROM lifecycle_provider_items
                 WHERE status = 'pending'
                   AND (locked_until IS NULL OR locked_until < now())
                   AND next_attempt_at <= now()
                   AND ($1::uuid IS NULL OR workspace_id = $1)
                 ORDER BY next_attempt_at LIMIT 20
                 FOR UPDATE SKIP LOCKED)
          RETURNING item, provider_id, workspace_id, request_id, attempts",
    )
    .bind(ws)
    .bind(LEASE_SECS)
    .fetch_all(&state.db)
    .await;
    rows.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "lifecycle provider: claim failed");
        Vec::new()
    })
}

async fn process(state: &AppState, paddle: &PaddleConfig, item: Claimed) {
    let (kind, id, ws, request_id, attempts) = item;
    let outcome = match kind.as_str() {
        "subscription" => cancel_subscription(paddle, &id).await,
        "checkout" => match cancel_checkout(paddle, &id).await {
            Ok((status, Some(sub))) => {
                // Paid in the meantime: its subscription must be canceled too.
                match register(state, ws, request_id, ProviderItem::Subscription, &sub).await {
                    Ok(()) => Outcome::Confirmed(status),
                    Err(e) => Outcome::Retry(format!("could not register {sub}: {}", e.message)),
                }
            }
            Ok((status, None)) => Outcome::Confirmed(status),
            Err(why) => Outcome::Retry(why),
        },
        other => Outcome::Retry(format!("unknown item kind {other}")),
    };
    let result = match outcome {
        Outcome::Confirmed(status) => {
            let r = sqlx::query(
                "UPDATE lifecycle_provider_items
                    SET status = 'confirmed', provider_status = $3, confirmed_at = now(),
                        attempts = attempts + 1, last_error = NULL, locked_until = NULL
                  WHERE item = $1 AND provider_id = $2",
            )
            .bind(&kind)
            .bind(&id)
            .bind(&status)
            .execute(&state.db)
            .await;
            if kind == "checkout" && status == "canceled" {
                // The local checkout row (if the tenant rows still exist)
                // must never be offered again.
                let _ = sqlx::query(
                    "UPDATE billing_checkouts SET status = 'canceled', updated_at = now()
                      WHERE transaction_id = $1 AND status = 'open'",
                )
                .bind(&id)
                .execute(&state.db)
                .await;
            }
            r
        }
        Outcome::Retry(why) => {
            let backoff = 2i64
                .saturating_pow((attempts + 1).min(12) as u32)
                .min(MAX_BACKOFF_SECS);
            tracing::warn!(item = %kind, provider_id = %id, workspace = %ws,
                attempt = attempts + 1, retry_in_secs = backoff, reason = %why,
                "lifecycle provider: cancellation not confirmed, will retry");
            sqlx::query(
                "UPDATE lifecycle_provider_items
                    SET attempts = attempts + 1, last_error = $3,
                        next_attempt_at = now() + make_interval(secs => $4),
                        locked_until = NULL
                  WHERE item = $1 AND provider_id = $2",
            )
            .bind(&kind)
            .bind(&id)
            .bind(why.chars().take(300).collect::<String>())
            .bind(backoff as f64)
            .execute(&state.db)
            .await
        }
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, "lifecycle provider: bookkeeping failed; lease expiry retries it");
    }
}

async fn get_entity(paddle: &PaddleConfig, path: &str) -> Result<serde_json::Value, String> {
    let resp = paddle_http()
        .get(format!("{}{path}", paddle.base_url))
        .bearer_auth(&paddle.api_key)
        .send()
        .await
        .map_err(|e| format!("paddle unreachable: {}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("paddle answered {status} reading the entity"));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid paddle json: {}", e.without_url()))?;
    body.get("data")
        .cloned()
        .ok_or_else(|| "paddle response without data".to_string())
}

async fn cancel_subscription(paddle: &PaddleConfig, id: &str) -> Outcome {
    let path = format!("/subscriptions/{id}");
    let before = match get_entity(paddle, &path).await {
        Ok(e) => e,
        Err(why) => return Outcome::Retry(why),
    };
    if before["id"].as_str() != Some(id) {
        return Outcome::Retry("paddle returned a different subscription".into());
    }
    if before["status"].as_str() == Some("canceled") {
        return Outcome::Confirmed("canceled".into());
    }
    let resp = paddle_http()
        .post(format!("{}{path}/cancel", paddle.base_url))
        .bearer_auth(&paddle.api_key)
        .json(&serde_json::json!({ "effective_from": "immediately" }))
        .send()
        .await;
    match resp {
        Err(e) => return Outcome::Retry(format!("paddle unreachable: {}", e.without_url())),
        Ok(r) if !r.status().is_success() => {
            return Outcome::Retry(format!(
                "paddle refused the cancellation with {}",
                r.status()
            ))
        }
        Ok(_) => {}
    }
    // Only the canonical entity confirms, never the cancel response.
    match get_entity(paddle, &path).await {
        Ok(e) if e["id"].as_str() == Some(id) && e["status"].as_str() == Some("canceled") => {
            Outcome::Confirmed("canceled".into())
        }
        Ok(e) => Outcome::Retry(format!(
            "paddle still reports the subscription {}",
            e["status"].as_str().unwrap_or("without status")
        )),
        Err(why) => Outcome::Retry(why),
    }
}

/// `Ok((status, paid_subscription))` when the checkout can no longer be
/// paid: canceled, or already paid (then the subscription it created).
async fn cancel_checkout(
    paddle: &PaddleConfig,
    id: &str,
) -> Result<(String, Option<String>), String> {
    let path = format!("/transactions/{id}");
    // Its answer is not trusted either way: the canonical read decides.
    let _ = paddle_http()
        .patch(format!("{}{path}", paddle.base_url))
        .bearer_auth(&paddle.api_key)
        .json(&serde_json::json!({ "status": "canceled" }))
        .send()
        .await;
    let txn = get_entity(paddle, &path).await?;
    if txn["id"].as_str() != Some(id) {
        return Err("paddle returned a different transaction".into());
    }
    match txn["status"].as_str().unwrap_or_default() {
        "canceled" => Ok(("canceled".into(), None)),
        st @ ("completed" | "billed" | "paid") => match txn["subscription_id"].as_str() {
            Some(sub) if sub.starts_with("sub_") => Ok((st.to_string(), Some(sub.to_string()))),
            _ => Err(format!(
                "transaction {st} without its subscription yet; retrying"
            )),
        },
        other => Err(format!("paddle still reports the checkout {other}")),
    }
}

/// Journals `provider_id` as an item of the deletion request of `ws` (once)
/// and projects it, under the workspace row lock.
async fn register(
    state: &AppState,
    ws: Uuid,
    request_id: Uuid,
    item: ProviderItem,
    provider_id: &str,
) -> Result<(), AppError> {
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(ws)
        .execute(&mut *tx)
        .await?;
    let mut authority = control::open_for_write(&state.config.control, &mut tx)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "lifecycle authority unavailable to register a provider item");
            AppError::not_ready("lifecycle authority unavailable")
        })?;
    if !authority
        .deletion_items(ws)
        .iter()
        .any(|(k, id)| *k == item && id == provider_id)
    {
        authority
            .append_projected(
                &mut tx,
                Record::DeletionProviderItem {
                    workspace_id: ws,
                    request_id,
                    item,
                    provider_id: provider_id.to_string(),
                },
                None,
            )
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "could not journal a provider item");
                AppError::not_ready("lifecycle authority unavailable")
            })?;
    }
    tx.commit().await?;
    drop(authority);
    Ok(())
}

/// Called by the canonical subscription reconciliation once Paddle shows
/// `sub_id` live (not canceled) for `ws`: when `ws` has a deletion request,
/// the subscription is registered and canceled immediately.
pub async fn register_live_subscription(
    state: &AppState,
    ws: Uuid,
    sub_id: &str,
) -> Result<(), AppError> {
    let request: Option<(Uuid,)> =
        sqlx::query_as("SELECT request_id FROM workspace_lifecycle WHERE workspace_id = $1")
            .bind(ws)
            .fetch_optional(&state.db)
            .await?;
    let Some((request_id,)) = request else {
        return Ok(());
    };
    register(state, ws, request_id, ProviderItem::Subscription, sub_id).await?;
    // A previously confirmed cancellation that Paddle reports live again
    // goes back to pending.
    sqlx::query(
        "UPDATE lifecycle_provider_items
            SET status = 'pending', confirmed_at = NULL, next_attempt_at = now()
          WHERE item = 'subscription' AND provider_id = $1",
    )
    .bind(sub_id)
    .execute(&state.db)
    .await?;
    attempt_workspace(state, ws).await;
    Ok(())
}
