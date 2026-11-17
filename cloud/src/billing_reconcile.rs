//! Canonical reconciliation of Paddle subscriptions.
//!
//! Webhooks and billing actions only enqueue a job (durably, in PostgreSQL).
//! A job fetches the subscription from Paddle (`GET /subscriptions/{id}`)
//! and writes entitlements from that canonical entity:
//! - the workspace binding comes from server-side state (the checkout
//!   transaction this server created, or an existing binding);
//! - the plan comes from the configured price ids, seats from the quantity;
//!   when no item carries a configured price (the operator rotated
//!   `MUSTER_PADDLE_PRICE_*` after the customer paid), a subscription Paddle
//!   created from a checkout this workspace recorded keeps that checkout's
//!   plan, so the payment is honoured instead of offering a second checkout;
//! - a customer different from the one already bound to the workspace or
//!   subscription grants nothing;
//! - an entity older (`updated_at`) than the one already applied is ignored,
//!   so duplicate, out-of-order and equal-timestamp events converge.
//!
//! Failed fetches are retried with exponential backoff, also after restarts.

use std::sync::OnceLock;
use std::time::Duration;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::config::PaddleConfig;
use crate::error::AppError;
use crate::state::AppState;

/// Longest wait between two attempts for the same subscription.
const MAX_BACKOFF_SECS: i64 = 3600;
/// From this many failed attempts on, a retried job is logged as an error.
const LOUD_AFTER_ATTEMPTS: i32 = 8;
/// How long a claimed job stays reserved for one worker.
const LEASE_SECS: i64 = 60;

pub(crate) fn paddle_http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client")
    })
}

/// Records that `sub_id` (bound to `ws`) must be re-read from Paddle.
/// Idempotent: repeated calls coalesce into one pending job.
pub async fn enqueue(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sub_id: &str,
    ws: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO billing_reconcile_jobs (paddle_subscription_id, workspace_id)
         VALUES ($1, $2)
         ON CONFLICT (paddle_subscription_id) DO UPDATE SET
           generation = billing_reconcile_jobs.generation + 1,
           next_attempt_at = now(),
           updated_at = now()
         WHERE billing_reconcile_jobs.workspace_id = EXCLUDED.workspace_id",
    )
    .bind(sub_id)
    .bind(ws)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Why a canonical subscription was not applied.
#[derive(Debug)]
enum Failure {
    /// Worth retrying (network, 5xx, rate limit, database).
    Transient(String),
    /// The entity cannot grant rights to this workspace; retrying won't help.
    Refused(String),
}

impl From<sqlx::Error> for Failure {
    fn from(e: sqlx::Error) -> Self {
        Failure::Transient(format!("database: {e}"))
    }
}

/// Tries the job for `sub_id` now if no other worker holds it. Errors are
/// recorded on the job, never returned: the caller has already committed.
pub async fn run_now(state: &AppState, sub_id: &str) {
    let claimed: Option<(Uuid, i64, i32)> = match sqlx::query_as(
        "UPDATE billing_reconcile_jobs
            SET locked_until = now() + make_interval(secs => $2)
          WHERE paddle_subscription_id = $1
            AND (locked_until IS NULL OR locked_until < now())
          RETURNING workspace_id, generation, attempts",
    )
    .bind(sub_id)
    .bind(LEASE_SECS as f64)
    .fetch_optional(&state.db)
    .await
    {
        Ok(row) => row,
        Err(e) => {
            tracing::warn!(error = %e, "billing reconcile: claim failed");
            return;
        }
    };
    if let Some((ws, generation, attempts)) = claimed {
        process(state, sub_id, ws, generation, attempts).await;
    }
}

/// Runs every due job once. Returns how many were attempted.
pub async fn run_due(state: &AppState, limit: i64) -> usize {
    // A fenced instance claims nothing and calls no provider.
    if !state
        .role_gate
        .allows(&state.config.control, &state.db, "billing reconcile")
        .await
    {
        return 0;
    }
    let due: Vec<(String, Uuid, i64, i32)> = match sqlx::query_as(
        "UPDATE billing_reconcile_jobs
            SET locked_until = now() + make_interval(secs => $2)
          WHERE paddle_subscription_id IN (
                SELECT paddle_subscription_id FROM billing_reconcile_jobs
                 WHERE next_attempt_at <= now()
                   AND (locked_until IS NULL OR locked_until < now())
                 ORDER BY next_attempt_at
                 LIMIT $1
                 FOR UPDATE SKIP LOCKED)
          RETURNING paddle_subscription_id, workspace_id, generation, attempts",
    )
    .bind(limit)
    .bind(LEASE_SECS as f64)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "billing reconcile: claim failed");
            return 0;
        }
    };
    let n = due.len();
    for (sub_id, ws, generation, attempts) in due {
        if !state
            .role_gate
            .allows(&state.config.control, &state.db, "billing reconcile")
            .await
        {
            break;
        }
        process(state, &sub_id, ws, generation, attempts).await;
    }
    n
}

/// Background loop: retries pending reconciliations, including jobs left by
/// a previous process.
pub async fn worker(state: AppState, every: Duration) {
    loop {
        if state.config.paddle.is_some() {
            run_due(&state, 20).await;
        }
        tokio::time::sleep(every).await;
    }
}

async fn process(state: &AppState, sub_id: &str, ws: Uuid, generation: i64, attempts: i32) {
    let Some(paddle) = &state.config.paddle else {
        return;
    };
    let outcome = match fetch(paddle, sub_id).await {
        Ok(entity) => apply(state, paddle, sub_id, ws, &entity).await,
        Err(f) => Err(f),
    };
    let result = match outcome {
        Ok(()) => finish(state, sub_id, ws, generation).await,
        Err(Failure::Refused(why)) => {
            tracing::warn!(subscription = sub_id, workspace = %ws, reason = %why,
                "billing reconcile: subscription refused, no entitlement change");
            finish(state, sub_id, ws, generation).await
        }
        Err(Failure::Transient(why)) => {
            let backoff = 2i64
                .saturating_pow((attempts + 1).min(12) as u32)
                .min(MAX_BACKOFF_SECS);
            if attempts + 1 >= LOUD_AFTER_ATTEMPTS {
                // Long stuck (Paddle down for long, or a checkout
                // transaction it keeps not returning): make it visible.
                tracing::error!(subscription = sub_id, workspace = %ws, attempts = attempts + 1,
                    retry_in_secs = backoff, reason = %why,
                    "billing reconcile: still failing after many attempts; will retry");
            } else {
                tracing::warn!(subscription = sub_id, attempt = attempts + 1,
                    retry_in_secs = backoff, reason = %why, "billing reconcile: will retry");
            }
            sqlx::query(
                "UPDATE billing_reconcile_jobs
                    SET attempts = attempts + 1,
                        last_error = $2,
                        next_attempt_at = now() + make_interval(secs => $3),
                        locked_until = NULL,
                        updated_at = now()
                  WHERE paddle_subscription_id = $1",
            )
            .bind(sub_id)
            .bind(truncate(&why, 300))
            .bind(backoff as f64)
            .execute(&state.db)
            .await
            .map(|_| ())
        }
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, "billing reconcile: bookkeeping failed; lease expiry retries it");
    }
}

/// Done with the state seen at `generation`; a newer enqueue keeps the job due.
/// The paid checkouts of `ws` that created `sub_id` are settled: their
/// subscription is reconciled (granted or refused), so they no longer hold
/// back another checkout.
async fn finish(
    state: &AppState,
    sub_id: &str,
    ws: Uuid,
    generation: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE billing_checkouts SET status = 'completed', updated_at = now()
          WHERE subscription_id = $1 AND workspace_id = $2 AND status = 'paid'",
    )
    .bind(sub_id)
    .bind(ws)
    .execute(&state.db)
    .await?;
    let deleted = sqlx::query(
        "DELETE FROM billing_reconcile_jobs WHERE paddle_subscription_id = $1 AND generation = $2",
    )
    .bind(sub_id)
    .bind(generation)
    .execute(&state.db)
    .await?;
    if deleted.rows_affected() == 0 {
        sqlx::query(
            "UPDATE billing_reconcile_jobs SET locked_until = NULL, next_attempt_at = now()
              WHERE paddle_subscription_id = $1",
        )
        .bind(sub_id)
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

async fn fetch(paddle: &PaddleConfig, sub_id: &str) -> Result<serde_json::Value, Failure> {
    fetch_entity(paddle, "subscriptions", sub_id).await
}

/// `GET /{kind}/{id}` from Paddle: its `data`. Not found is final; anything
/// else unanswered is retried.
async fn fetch_entity(
    paddle: &PaddleConfig,
    kind: &str,
    id: &str,
) -> Result<serde_json::Value, Failure> {
    let resp = paddle_http()
        .get(format!("{}/{kind}/{id}", paddle.base_url))
        .bearer_auth(&paddle.api_key)
        .send()
        .await
        .map_err(|e| Failure::Transient(format!("paddle unreachable: {}", e.without_url())))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(Failure::Refused(format!(
            "{kind} entity not found at paddle"
        )));
    }
    if !status.is_success() {
        return Err(Failure::Transient(format!("paddle answered {status}")));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Failure::Transient(format!("invalid paddle json: {}", e.without_url())))?;
    body.get("data")
        .cloned()
        .ok_or_else(|| Failure::Transient("paddle response without data".into()))
}

/// The fields of a canonical subscription this service relies on.
struct Canonical {
    customer_id: String,
    status: String,
    /// From the configured price ids; `None` when no item carries one.
    plan_seats: Option<(&'static str, i32)>,
    period_start: Option<OffsetDateTime>,
    period_end: Option<OffsetDateTime>,
    cancel_at_period_end: bool,
    canceled_at: Option<OffsetDateTime>,
    updated_at: OffsetDateTime,
}

fn ts(v: &serde_json::Value) -> Option<OffsetDateTime> {
    v.as_str()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
}

fn parse(paddle: &PaddleConfig, sub_id: &str, e: &serde_json::Value) -> Result<Canonical, Failure> {
    let refuse = |why: &str| Failure::Refused(why.to_string());
    if e["id"].as_str() != Some(sub_id) {
        return Err(refuse("paddle returned a different subscription id"));
    }
    let customer_id = e["customer_id"]
        .as_str()
        .filter(|c| c.starts_with("ctm_"))
        .ok_or_else(|| refuse("subscription without customer"))?
        .to_string();
    let status = e["status"].as_str().unwrap_or_default();
    if !matches!(
        status,
        "active" | "trialing" | "past_due" | "paused" | "canceled"
    ) {
        return Err(refuse("unknown subscription status"));
    }
    let mut plan_seats = None;
    for item in e["items"].as_array().into_iter().flatten() {
        let price = item["price"]["id"].as_str().unwrap_or_default();
        let quantity = item["quantity"].as_i64().unwrap_or(0);
        if price == paddle.price_team_seat && quantity >= 1 {
            plan_seats = Some(("team", quantity.min(i32::MAX as i64) as i32));
        } else if price == paddle.price_personal && plan_seats.is_none() {
            plan_seats = Some(("personal", 1));
        }
    }
    let updated_at =
        ts(&e["updated_at"]).ok_or_else(|| refuse("subscription without updated_at"))?;
    Ok(Canonical {
        customer_id,
        status: status.to_string(),
        plan_seats,
        period_start: ts(&e["current_billing_period"]["starts_at"]),
        period_end: ts(&e["current_billing_period"]["ends_at"]),
        cancel_at_period_end: e["scheduled_change"]["action"].as_str() == Some("cancel"),
        canceled_at: ts(&e["canceled_at"]),
        updated_at,
    })
}

/// The plan of a subscription none of whose items carries a configured
/// price, for instance because the operator rotated `MUSTER_PADDLE_PRICE_*`
/// between the payment and this reconciliation. It is granted only when it
/// provably comes from a checkout workspace `ws` recorded itself: that
/// checkout names it, Paddle's transaction for that checkout created it for
/// the same customer, and one of the subscription's items carries the price
/// the checkout was created with. The plan is the checkout's; a team's seats are Paddle's
/// quantity. The customer association rules of [`apply`] still follow.
/// Anything else stays refused, and nothing is ever changed at Paddle. A
/// transaction Paddle does not return (any failure, `404` included) proves
/// nothing either way: the job is retried, never settled, so the payment
/// keeps blocking a second checkout instead of being dropped.
async fn origin_plan(
    state: &AppState,
    paddle: &PaddleConfig,
    sub_id: &str,
    ws: Uuid,
    customer_id: &str,
    entity: &serde_json::Value,
) -> Result<(&'static str, i32), Failure> {
    let refuse =
        |why: &str| Failure::Refused(format!("no item with a configured price, and {why}"));
    let origin: Option<(String, String, String)> = sqlx::query_as(
        "SELECT transaction_id, plan, price_id FROM billing_checkouts
          WHERE workspace_id = $1 AND subscription_id = $2
          ORDER BY created_at DESC
          LIMIT 1",
    )
    .bind(ws)
    .bind(sub_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((txn_id, plan, price_id)) = origin else {
        return Err(refuse("no checkout of this workspace created it"));
    };
    let txn = fetch_entity(paddle, "transactions", &txn_id)
        .await
        .map_err(|f| match f {
            Failure::Refused(why) | Failure::Transient(why) => {
                Failure::Transient(format!("checkout transaction unreadable: {why}"))
            }
        })?;
    if txn["id"].as_str() != Some(txn_id.as_str())
        || txn["subscription_id"].as_str() != Some(sub_id)
    {
        return Err(refuse("Paddle's checkout transaction did not create it"));
    }
    // Fails closed: a transaction that names no customer proves nothing.
    if txn["customer_id"].as_str() != Some(customer_id) {
        return Err(refuse(
            "its checkout transaction was not paid by the subscription's customer",
        ));
    }
    let quantity = entity["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["price"]["id"].as_str() == Some(price_id.as_str()))
        .filter_map(|item| item["quantity"].as_i64())
        .find(|q| *q >= 1)
        .ok_or_else(|| refuse("no item carries its checkout's price"))?;
    let plan_seats = match plan.as_str() {
        "team" => ("team", quantity.min(i32::MAX as i64) as i32),
        "personal" => ("personal", 1),
        _ => return Err(refuse("its checkout has an unknown plan")),
    };
    tracing::info!(subscription = sub_id, workspace = %ws, transaction = %txn_id,
        plan = plan_seats.0,
        "billing reconcile: price no longer configured; granted from this workspace's checkout");
    Ok(plan_seats)
}

/// The plan of a subscription with no configured price that is already
/// registered for workspace `ws` and customer `customer_id` (its origin was
/// proven when it was first granted): the plan stored then; a team's seats
/// are Paddle's quantity of the item carrying its checkout's price. When no
/// item carries that price any more (moved at Paddle to another price that
/// is not configured either), nothing is guessed: the stored plan and seats
/// are kept. Either way it is logged as a warning. Its later changes (a
/// cancellation, a seat change) are applied without proving its origin
/// again, so they never wait on, or are lost to, its checkout transaction.
/// `None` when it is not registered so.
async fn bound_plan(
    state: &AppState,
    sub_id: &str,
    ws: Uuid,
    customer_id: &str,
    entity: &serde_json::Value,
) -> Result<Option<(&'static str, i32)>, Failure> {
    let stored: Option<(String, i32)> = sqlx::query_as(
        "SELECT plan, seats FROM subscriptions
          WHERE paddle_subscription_id = $1 AND workspace_id = $2 AND customer_id = $3",
    )
    .bind(sub_id)
    .bind(ws)
    .bind(customer_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((plan, stored_seats)) = stored else {
        return Ok(None);
    };
    let plan: &'static str = match plan.as_str() {
        "team" => "team",
        "personal" => "personal",
        _ => return Ok(None),
    };
    let checkout_price: Option<(String,)> = sqlx::query_as(
        "SELECT price_id FROM billing_checkouts
          WHERE workspace_id = $1 AND subscription_id = $2
          ORDER BY created_at DESC
          LIMIT 1",
    )
    .bind(ws)
    .bind(sub_id)
    .fetch_optional(&state.db)
    .await?;
    let quantity = checkout_price.and_then(|(price,)| {
        entity["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["price"]["id"].as_str() == Some(price.as_str()))
            .filter_map(|item| item["quantity"].as_i64())
            .find(|q| *q >= 1)
    });
    let seats = match quantity {
        Some(q) => {
            tracing::warn!(subscription = sub_id, workspace = %ws, plan,
                "billing reconcile: price no longer configured; plan kept as registered, \
                 seats from the item carrying its checkout's price");
            if plan == "team" {
                q.min(i32::MAX as i64) as i32
            } else {
                1
            }
        }
        None => {
            tracing::warn!(subscription = sub_id, workspace = %ws, plan, seats = stored_seats,
                "billing reconcile: no item carries a configured price or its checkout's \
                 price (moved at Paddle?); plan and seats kept as registered");
            stored_seats
        }
    };
    Ok(Some((plan, seats)))
}

async fn apply(
    state: &AppState,
    paddle: &PaddleConfig,
    sub_id: &str,
    ws: Uuid,
    entity: &serde_json::Value,
) -> Result<(), Failure> {
    let c = parse(paddle, sub_id, entity)?;
    let (plan, seats) = match c.plan_seats {
        Some(plan_seats) => plan_seats,
        None => match bound_plan(state, sub_id, ws, &c.customer_id, entity).await? {
            Some(plan_seats) => plan_seats,
            None => origin_plan(state, paddle, sub_id, ws, &c.customer_id, entity).await?,
        },
    };
    let mut tx = state.db.begin().await?;

    type Existing = (Uuid, String, Option<OffsetDateTime>);
    let existing: Option<Existing> = sqlx::query_as(
        "SELECT workspace_id, customer_id, provider_updated_at FROM subscriptions
          WHERE paddle_subscription_id = $1 FOR UPDATE",
    )
    .bind(sub_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((bound_ws, bound_customer, applied_at)) = &existing {
        if *bound_ws != ws {
            return Err(Failure::Refused(
                "subscription bound to another workspace".into(),
            ));
        }
        if *bound_customer != c.customer_id {
            return Err(Failure::Refused(
                "customer differs from the bound customer".into(),
            ));
        }
        if applied_at.is_some_and(|t| t > c.updated_at) {
            // An entity newer than this one is already applied.
            tx.commit().await?;
            return Ok(());
        }
    }
    let ws_customer: Option<(String,)> = sqlx::query_as(
        "SELECT customer_id FROM billing_customers WHERE workspace_id = $1 FOR UPDATE",
    )
    .bind(ws)
    .fetch_optional(&mut *tx)
    .await?;
    match ws_customer {
        Some((bound,)) if bound != c.customer_id => {
            return Err(Failure::Refused(
                "workspace already bound to another customer".into(),
            ));
        }
        Some(_) => {}
        None => {
            sqlx::query(
                "INSERT INTO billing_customers (workspace_id, customer_id, email) VALUES ($1, $2, '')",
            )
            .bind(ws)
            .bind(&c.customer_id)
            .execute(&mut *tx)
            .await?;
        }
    }

    sqlx::query(
        "INSERT INTO subscriptions (workspace_id, paddle_subscription_id, customer_id, plan, seats,
                status, current_period_start, current_period_end, cancel_at_period_end,
                canceled_at, provider_updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, now()), COALESCE($8, $7, now()), $9, $10, $11)
         ON CONFLICT (paddle_subscription_id) DO UPDATE SET
           plan = EXCLUDED.plan,
           seats = EXCLUDED.seats,
           status = EXCLUDED.status,
           current_period_start = COALESCE($7, subscriptions.current_period_start),
           current_period_end = COALESCE($8, subscriptions.current_period_end),
           cancel_at_period_end = EXCLUDED.cancel_at_period_end,
           canceled_at = EXCLUDED.canceled_at,
           provider_updated_at = EXCLUDED.provider_updated_at,
           updated_at = now()",
    )
    .bind(ws)
    .bind(sub_id)
    .bind(&c.customer_id)
    .bind(plan)
    .bind(seats)
    .bind(&c.status)
    .bind(c.period_start)
    .bind(c.period_end)
    .bind(c.cancel_at_period_end)
    .bind(c.canceled_at)
    .bind(c.updated_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if c.status != "canceled" {
        // Live after a deletion request (late completion, reactivation):
        // cancel it immediately too. A failure retries this job.
        crate::lifecycle_provider::register_live_subscription(state, ws, sub_id)
            .await
            .map_err(|e| Failure::Transient(format!("deletion cancellation: {}", e.message)))?;
    }
    Ok(())
}
