//! Billing and subscription management (Paddle Billing).
//!
//! Subscriptions and transactions are created and verified server-side.
//! Browser redirects never assign rights. When billing is not configured,
//! workspaces remain on their 14-day free trial; mutating billing endpoints
//! return `503 billing_unconfigured`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::account::Account;
use crate::error::AppError;
use crate::protocol::{
    error_codes, scopes, CheckoutSession, CreateCheckout, CustomerPortalSession, PlanStatus,
    UpdatePlan, WorkspaceBilling,
};
use crate::state::AppState;
use crate::tenancy::parse_json;

type HmacSha256 = Hmac<Sha256>;

fn rfc3339(t: OffsetDateTime) -> Result<String, AppError> {
    t.format(&Rfc3339).map_err(AppError::internal)
}

fn parse_uuid(s: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(s).map_err(|_| AppError::not_found())
}

async fn check_role(state: &AppState, ws: Uuid, user: Uuid) -> Result<String, AppError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2")
            .bind(ws)
            .bind(user)
            .fetch_optional(&state.db)
            .await?;
    let (role,) = row.ok_or_else(AppError::not_found)?;
    Ok(role)
}

async fn require_admin_or_owner(state: &AppState, ws: Uuid, user: Uuid) -> Result<(), AppError> {
    let role = check_role(state, ws, user).await?;
    if role != "owner" && role != "admin" {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            error_codes::FORBIDDEN_ROLE,
            "only owners and admins can manage billing",
        ));
    }
    Ok(())
}

/// `(plan, status, seats, current_period_end, cancel_at_period_end)`.
pub(crate) type SubRow = (String, String, i32, OffsetDateTime, bool);

/// The subscription that decides a workspace's entitlements: a workspace can
/// accumulate several (canceled, then subscribed again); a live one wins over
/// a canceled one, then the most recently updated by Paddle.
pub(crate) async fn current_subscription<'e>(
    db: impl sqlx::PgExecutor<'e>,
    ws: Uuid,
) -> Result<Option<SubRow>, AppError> {
    Ok(sqlx::query_as(
        "SELECT plan, status, seats, current_period_end, cancel_at_period_end
           FROM subscriptions WHERE workspace_id = $1
          ORDER BY (status <> 'canceled') DESC, provider_updated_at DESC NULLS LAST, updated_at DESC
          LIMIT 1",
    )
    .bind(ws)
    .fetch_optional(db)
    .await?)
}

/// What a Paddle subscription status allows, with no locally invented
/// windows. `past_due` keeps push while Paddle retries the payment (Paddle
/// then cancels or pauses it); pull and export never depend on payment.
pub(crate) fn subscription_entitlement(status: &str) -> (PlanStatus, bool) {
    match status {
        "active" | "trialing" => (PlanStatus::Active, true),
        "past_due" => (PlanStatus::PastDue, true),
        "canceled" => (PlanStatus::Canceled, false),
        _ => (PlanStatus::Restricted, false),
    }
}

/// The limits a workspace's plan declares in `GET /billing`. The single
/// source for that view and for every guard that enforces them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlanLimits {
    pub max_devices: u32,
    pub max_bundle_bytes: usize,
    pub max_storage_bytes: u64,
}

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
/// The largest `max_bundle_bytes` of any plan.
pub(crate) const MAX_PLAN_BUNDLE_BYTES: usize = (25 * MIB) as usize;

/// Limits for the subscription that decides entitlements (see
/// [`current_subscription`]); no subscription means the trial.
fn plan_limits(sub: Option<&SubRow>) -> PlanLimits {
    match sub {
        Some((plan, _, seats, _, _)) if plan == "team" => {
            let seats = *seats as u32;
            PlanLimits {
                max_devices: seats * 50,
                max_bundle_bytes: MAX_PLAN_BUNDLE_BYTES,
                max_storage_bytes: seats as u64 * 100 * GIB,
            }
        }
        Some(_) => PlanLimits {
            max_devices: 10,
            max_bundle_bytes: MAX_PLAN_BUNDLE_BYTES,
            max_storage_bytes: 10 * GIB,
        },
        None => PlanLimits {
            max_devices: 3,
            max_bundle_bytes: (10 * MIB) as usize,
            max_storage_bytes: GIB,
        },
    }
}

/// The limits in force: [`plan_limits`] plus the development-only storage
/// override. Every view and guard goes through here.
pub(crate) fn limits_for(config: &crate::config::Config, sub: Option<&SubRow>) -> PlanLimits {
    let mut limits = plan_limits(sub);
    if let Some(bytes) = config.dev_storage_limit {
        limits.max_storage_bytes = bytes;
    }
    limits
}

/// [`limits_for`] the workspace's current subscription.
pub(crate) async fn workspace_limits<'e>(
    config: &crate::config::Config,
    db: impl sqlx::PgExecutor<'e>,
    ws: Uuid,
) -> Result<PlanLimits, AppError> {
    Ok(limits_for(
        config,
        current_subscription(db, ws).await?.as_ref(),
    ))
}

/// What the workspace consumes against `max_storage_bytes`: its Git
/// storage plus what declarative proposals keep in the database. `db` is
/// the caller's connection: a guard holding the workspace lock must not
/// wait for a second pool connection (others may hold them all while
/// waiting for that lock).
pub(crate) async fn storage_used<'e>(
    state: &AppState,
    db: impl sqlx::PgExecutor<'e>,
    ws: Uuid,
) -> Result<u64, AppError> {
    let repos = state.repos.clone();
    let git = tokio::task::spawn_blocking(move || repos.usage(ws))
        .await
        .map_err(AppError::internal)??;
    let proposals: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(octet_length(payload::text)), 0)::int8
           FROM proposals WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_one(db)
    .await?;
    Ok(git.saturating_add(proposals.max(0) as u64))
}

/// Refuses with `507 quota_exceeded` when storing `extra` more bytes would
/// take the workspace past its `max_storage_bytes`. Locks the workspace row
/// first, so concurrent writers that all check here cannot overshoot
/// together; the caller writes within the same transaction or lock.
pub(crate) async fn ensure_storage_fits(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ws: Uuid,
    extra: u64,
) -> Result<(), AppError> {
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(ws)
        .execute(&mut **tx)
        .await?;
    let limits = workspace_limits(&state.config, &mut **tx, ws).await?;
    let used = storage_used(state, &mut **tx, ws).await?;
    if used.saturating_add(extra) > limits.max_storage_bytes {
        return Err(AppError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            error_codes::QUOTA_EXCEEDED,
            "the workspace plan's storage limit would be exceeded",
        )
        .with_details(serde_json::json!({
            "limit": "max_storage_bytes",
            "max_storage_bytes": limits.max_storage_bytes,
            "storage_used_bytes": used,
        })));
    }
    Ok(())
}

/// `GET /v1/workspaces/{ws}/billing`: plan, trial, limits, and capabilities.
pub async fn get_billing(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
) -> Result<Json<WorkspaceBilling>, AppError> {
    account.require_scope(scopes::WORKSPACES_READ)?;
    let ws = parse_uuid(&ws_str)?;
    let _ = check_role(&state, ws, account.user_id).await?;

    let ws_row: Option<(OffsetDateTime,)> =
        sqlx::query_as("SELECT trial_ends_at FROM workspaces WHERE id = $1")
            .bind(ws)
            .fetch_optional(&state.db)
            .await?;
    let (trial_ends_at,) = ws_row.ok_or_else(AppError::not_found)?;

    let now = OffsetDateTime::now_utc();
    let billing_configured = state.config.paddle.is_some();

    let sub = current_subscription(&state.db, ws).await?;
    let limits = limits_for(&state.config, sub.as_ref());
    let storage_used = storage_used(&state, &state.db, ws).await?;

    let billing = match sub {
        Some((plan, status_str, seats, period_end, cancel_at_end)) => {
            let (plan_status, can_push) = subscription_entitlement(&status_str);
            WorkspaceBilling {
                workspace_id: ws.to_string(),
                billing_configured,
                plan,
                plan_status,
                seats: seats as u32,
                trial_ends_at: None,
                current_period_end: Some(rfc3339(period_end)?),
                cancel_at_period_end: cancel_at_end,
                can_push,
                can_pull: true,
                can_export: true,
                max_devices: limits.max_devices,
                max_bundle_bytes: limits.max_bundle_bytes,
                max_storage_bytes: limits.max_storage_bytes,
                storage_used_bytes: storage_used,
            }
        }
        None => {
            let is_trialing = now < trial_ends_at;
            let plan_status = if is_trialing {
                PlanStatus::Trialing
            } else {
                PlanStatus::TrialExpired
            };
            WorkspaceBilling {
                workspace_id: ws.to_string(),
                billing_configured,
                plan: "trial".into(),
                plan_status,
                seats: 1,
                trial_ends_at: Some(rfc3339(trial_ends_at)?),
                current_period_end: None,
                cancel_at_period_end: false,
                can_push: is_trialing,
                can_pull: true,
                can_export: true,
                max_devices: limits.max_devices,
                max_bundle_bytes: limits.max_bundle_bytes,
                max_storage_bytes: limits.max_storage_bytes,
                storage_used_bytes: storage_used,
            }
        }
    };

    Ok(Json(billing))
}

/// `POST /v1/workspaces/{ws}/billing/checkout`: creates a Paddle checkout
/// transaction associated with this workspace.
pub async fn create_checkout(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<CheckoutSession>, AppError> {
    account.require_scope(scopes::WORKSPACES_WRITE)?;
    let ws = parse_uuid(&ws_str)?;
    require_admin_or_owner(&state, ws, account.user_id).await?;

    let Some(paddle) = &state.config.paddle else {
        return Err(AppError::billing_unconfigured());
    };

    let req: CreateCheckout = parse_json(&body)?;
    let is_team = req.plan == "team";
    let plan = if is_team { "team" } else { "personal" };
    let seats = if is_team { req.seats.max(1) } else { 1 };
    let price_id = if is_team {
        &paddle.price_team_seat
    } else {
        &paddle.price_personal
    };

    // One checkout decision at a time per workspace: concurrent requests
    // and retries see the same open checkout instead of each creating a
    // payable Paddle transaction.
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(ws)
        .fetch_one(&mut *tx)
        .await?;
    if current_subscription(&mut *tx, ws)
        .await?
        .is_some_and(|(_, status, ..)| status != "canceled")
    {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            error_codes::SUBSCRIPTION_EXISTS,
            "workspace already has a live subscription",
        ));
    }

    type OpenCheckout = (String, String, i32, Option<String>);
    let open: Option<OpenCheckout> = sqlx::query_as(
        "SELECT transaction_id, plan, seats, checkout_url FROM billing_checkouts
          WHERE workspace_id = $1 AND status = 'open'",
    )
    .bind(ws)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((txn_id, open_plan, open_seats, Some(url))) = &open {
        if open_plan == plan && *open_seats == seats as i32 {
            tx.commit().await?;
            return Ok(Json(CheckoutSession {
                checkout_url: url.clone(),
                transaction_id: txn_id.clone(),
            }));
        }
    }
    if let Some((txn_id, ..)) = &open {
        // A different plan: the old payment link must stop being payable
        // before a new one exists (Paddle: PATCH status=canceled, alone).
        let resp = crate::billing_reconcile::paddle_http()
            .patch(format!("{}/transactions/{txn_id}", paddle.base_url))
            .bearer_auth(&paddle.api_key)
            .json(&serde_json::json!({ "status": "canceled" }))
            .send()
            .await
            .map_err(|e| {
                AppError::internal(format!(
                    "paddle transaction cancel error: {}",
                    e.without_url()
                ))
            })?;
        if !resp.status().is_success() {
            // Paddle does not name the refusal; read the transaction instead.
            let paid = transaction_status(paddle, txn_id)
                .await
                .is_some_and(|st| st == "completed" || st == "billed");
            if paid {
                // Paid: its subscription is on the way and the association
                // stays; never offer this link again nor a second checkout.
                sqlx::query(
                    "UPDATE billing_checkouts SET status = 'completed', updated_at = now()
                      WHERE transaction_id = $1",
                )
                .bind(txn_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                return Err(AppError::new(
                    StatusCode::CONFLICT,
                    error_codes::SUBSCRIPTION_EXISTS,
                    "the open checkout was already paid; its subscription is being set up",
                ));
            }
            return Err(AppError::internal(format!(
                "paddle refused to cancel the open checkout with {}",
                resp.status()
            )));
        }
        sqlx::query(
            "UPDATE billing_checkouts SET status = 'canceled', updated_at = now()
              WHERE transaction_id = $1",
        )
        .bind(txn_id)
        .execute(&mut *tx)
        .await?;
    }

    let customer_row: Option<(String,)> =
        sqlx::query_as("SELECT customer_id FROM billing_customers WHERE workspace_id = $1")
            .bind(ws)
            .fetch_optional(&mut *tx)
            .await?;

    let mut payload = serde_json::json!({
        "items": [
            {
                "price_id": price_id,
                "quantity": seats
            }
        ],
        "custom_data": {
            "workspace_id": ws.to_string(),
            "user_id": account.user_id.to_string(),
            "plan": plan
        }
    });
    if let Some((cust_id,)) = customer_row {
        payload["customer_id"] = serde_json::Value::String(cust_id);
    }

    let resp = crate::billing_reconcile::paddle_http()
        .post(format!("{}/transactions", paddle.base_url))
        .bearer_auth(&paddle.api_key)
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            AppError::internal(format!(
                "paddle transactions api error: {}",
                e.without_url()
            ))
        })?;
    if !resp.status().is_success() {
        return Err(AppError::internal(format!(
            "paddle rejected transaction with {}",
            resp.status()
        )));
    }
    let resp_json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::internal(format!("invalid paddle json: {}", e.without_url())))?;
    let data = resp_json
        .get("data")
        .ok_or_else(|| AppError::internal("no data in paddle resp"))?;
    let txn_id = data
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|id| id.starts_with("txn_"))
        .ok_or_else(|| AppError::internal("no transaction id in paddle resp"))?
        .to_string();
    let checkout_url = data
        .get("checkout")
        .and_then(|c| c.get("url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| AppError::internal("no checkout url in paddle resp"))?
        .to_string();

    // The association that lets `subscription.created` grant rights.
    sqlx::query(
        "INSERT INTO billing_checkouts
                (transaction_id, workspace_id, created_by, plan, seats, price_id, status, checkout_url)
         VALUES ($1, $2, $3, $4, $5, $6, 'open', $7)",
    )
    .bind(&txn_id)
    .bind(ws)
    .bind(account.user_id)
    .bind(plan)
    .bind(seats as i32)
    .bind(price_id)
    .bind(&checkout_url)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(Json(CheckoutSession {
        checkout_url,
        transaction_id: txn_id,
    }))
}

/// Paddle's current status of transaction `txn_id` (`GET /transactions/{id}`),
/// `None` when it cannot be read.
async fn transaction_status(paddle: &crate::config::PaddleConfig, txn_id: &str) -> Option<String> {
    let resp = crate::billing_reconcile::paddle_http()
        .get(format!("{}/transactions/{txn_id}", paddle.base_url))
        .bearer_auth(&paddle.api_key)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body["data"]["status"].as_str().map(str::to_string)
}

/// `POST /v1/workspaces/{ws}/billing/portal`: customer portal session.
pub async fn customer_portal(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
) -> Result<Json<CustomerPortalSession>, AppError> {
    account.require_scope(scopes::WORKSPACES_WRITE)?;
    let ws = parse_uuid(&ws_str)?;
    require_admin_or_owner(&state, ws, account.user_id).await?;

    let Some(paddle) = &state.config.paddle else {
        return Err(AppError::billing_unconfigured());
    };

    let cust_row: Option<(String,)> =
        sqlx::query_as("SELECT customer_id FROM billing_customers WHERE workspace_id = $1")
            .bind(ws)
            .fetch_optional(&state.db)
            .await?;

    let (cust_id,) = cust_row
        .ok_or_else(|| AppError::bad_request("workspace has no billing customer associated yet"))?;

    let client = reqwest::Client::new();
    let url = format!("{}/customers/{cust_id}/portal-sessions", paddle.base_url);

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", paddle.api_key))
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| AppError::internal(format!("paddle portal error: {e}")))?;

    if !resp.status().is_success() {
        let err_text = resp.text().await.unwrap_or_default();
        return Err(AppError::internal(format!(
            "paddle portal rejected: {err_text}"
        )));
    }

    let resp_json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::internal(format!("invalid paddle json: {e}")))?;

    let portal_url = resp_json
        .get("data")
        .and_then(|d| d.get("urls"))
        .and_then(|u| u.get("general"))
        .and_then(|g| g.get("overview"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::internal("no portal overview url in paddle resp"))?
        .to_string();

    Ok(Json(CustomerPortalSession { portal_url }))
}

/// `POST /v1/workspaces/{ws}/billing/cancel`: cancel at end of billing period.
pub async fn cancel_subscription(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
) -> Result<Response, AppError> {
    account.require_scope(scopes::WORKSPACES_WRITE)?;
    let ws = parse_uuid(&ws_str)?;
    require_admin_or_owner(&state, ws, account.user_id).await?;

    let Some(paddle) = &state.config.paddle else {
        return Err(AppError::billing_unconfigured());
    };

    // The row lock serialises concurrent cancels of the same subscription.
    let mut tx = state.db.begin().await?;
    let sub_row: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT paddle_subscription_id, status, cancel_at_period_end FROM subscriptions
          WHERE workspace_id = $1
          ORDER BY (status <> 'canceled') DESC, provider_updated_at DESC NULLS LAST, updated_at DESC
          LIMIT 1
          FOR UPDATE",
    )
    .bind(ws)
    .fetch_optional(&mut *tx)
    .await?;

    let (sub_id, status, already_scheduled) = sub_row
        .ok_or_else(|| AppError::bad_request("workspace has no active subscription to cancel"))?;
    if already_scheduled || status == "canceled" {
        // Already cancelled or scheduled: nothing to ask Paddle again.
        tx.commit().await?;
        return Ok(StatusCode::OK.into_response());
    }

    let resp = crate::billing_reconcile::paddle_http()
        .post(format!("{}/subscriptions/{sub_id}/cancel", paddle.base_url))
        .bearer_auth(&paddle.api_key)
        .json(&serde_json::json!({ "effective_from": "next_billing_period" }))
        .send()
        .await
        .map_err(|e| AppError::internal(format!("paddle cancel error: {}", e.without_url())))?;

    if !resp.status().is_success() {
        return Err(AppError::internal(format!(
            "paddle cancel rejected with {}",
            resp.status()
        )));
    }

    // Local state comes from Paddle's canonical subscription, not from here.
    crate::billing_reconcile::enqueue(&mut tx, &sub_id, ws).await?;
    tx.commit().await?;
    crate::billing_reconcile::run_now(&state, &sub_id).await;

    Ok(StatusCode::OK.into_response())
}

/// `POST /v1/workspaces/{ws}/billing/plan`: update plan or seat count.
pub async fn update_plan(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, AppError> {
    account.require_scope(scopes::WORKSPACES_WRITE)?;
    let ws = parse_uuid(&ws_str)?;
    require_admin_or_owner(&state, ws, account.user_id).await?;

    let Some(paddle) = &state.config.paddle else {
        return Err(AppError::billing_unconfigured());
    };

    let req: UpdatePlan = parse_json(&body)?;
    let is_team = req.plan == "team";
    let seats = if is_team { req.seats.max(1) } else { 1 };
    let price_id = if is_team {
        &paddle.price_team_seat
    } else {
        &paddle.price_personal
    };

    let sub_row: Option<(String,)> = sqlx::query_as(
        "SELECT paddle_subscription_id FROM subscriptions
          WHERE workspace_id = $1 AND status <> 'canceled'
          ORDER BY provider_updated_at DESC NULLS LAST, updated_at DESC
          LIMIT 1",
    )
    .bind(ws)
    .fetch_optional(&state.db)
    .await?;

    let (sub_id,) = sub_row
        .ok_or_else(|| AppError::bad_request("workspace has no active subscription to update"))?;

    let resp = crate::billing_reconcile::paddle_http()
        .patch(format!("{}/subscriptions/{sub_id}", paddle.base_url))
        .bearer_auth(&paddle.api_key)
        .json(&serde_json::json!({
            "items": [
                {
                    "price_id": price_id,
                    "quantity": seats
                }
            ],
            "proration_billing_mode": "prorated_immediately"
        }))
        .send()
        .await
        .map_err(|e| AppError::internal(format!("paddle update error: {}", e.without_url())))?;

    if !resp.status().is_success() {
        return Err(AppError::internal(format!(
            "paddle update rejected with {}",
            resp.status()
        )));
    }

    // Entitlements follow Paddle's canonical subscription.
    let mut tx = state.db.begin().await?;
    crate::billing_reconcile::enqueue(&mut tx, &sub_id, ws).await?;
    tx.commit().await?;
    crate::billing_reconcile::run_now(&state, &sub_id).await;

    Ok(StatusCode::OK.into_response())
}

fn verify_paddle_signature(
    secret: &str,
    sig_header: &str,
    body: &[u8],
    max_skew_secs: i64,
) -> Result<(), AppError> {
    let mut ts_val: Option<i64> = None;
    let mut signatures: Vec<&str> = Vec::new();

    for part in sig_header.split(';') {
        let Some((k, v)) = part.trim().split_once('=') else {
            continue;
        };
        match k {
            "ts" => ts_val = v.parse::<i64>().ok(),
            "h1" => signatures.push(v),
            _ => {}
        }
    }

    let Some(ts) = ts_val else {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_SIGNATURE,
            "missing ts in Paddle-Signature",
        ));
    };

    let now = OffsetDateTime::now_utc().unix_timestamp();
    if (now - ts).abs() > max_skew_secs {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_SIGNATURE,
            "timestamp outside allowed window",
        ));
    }

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(AppError::internal)?;
    let ts_str = ts.to_string();
    mac.update(ts_str.as_bytes());
    mac.update(b":");
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    let expected_hex: String = expected.iter().map(|b| format!("{b:02x}")).collect();

    let matches = signatures
        .iter()
        .any(|sig| crate::auth::constant_time_eq(expected_hex.as_bytes(), sig.as_bytes()));

    if !matches {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            error_codes::INVALID_SIGNATURE,
            "invalid webhook signature",
        ));
    }

    Ok(())
}

/// Workspace an event belongs to, from server-side state only: a subscription
/// already bound to a workspace, or the checkout transaction this server
/// created (`transaction_id`, present on `subscription.created`).
/// `custom_data` is never consulted.
async fn associated_workspace(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    data: Option<&serde_json::Value>,
) -> Result<Option<Uuid>, AppError> {
    let Some(data) = data else { return Ok(None) };
    if let Some(sub_id) = data.get("id").and_then(|v| v.as_str()) {
        let bound: Option<(Uuid,)> = sqlx::query_as(
            "SELECT workspace_id FROM subscriptions WHERE paddle_subscription_id = $1",
        )
        .bind(sub_id)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some((ws,)) = bound {
            return Ok(Some(ws));
        }
    }
    let Some(txn) = data.get("transaction_id").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    let checkout: Option<(Uuid,)> =
        sqlx::query_as("SELECT workspace_id FROM billing_checkouts WHERE transaction_id = $1")
            .bind(txn)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(checkout.map(|(ws,)| ws))
}

/// `POST /v1/billing/webhook`: unauthenticated webhook endpoint for Paddle.
pub async fn handle_webhook(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<StatusCode, AppError> {
    let Some(paddle) = &state.config.paddle else {
        return Err(AppError::billing_unconfigured());
    };

    let sig_header = headers
        .get("Paddle-Signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            AppError::new(
                StatusCode::UNAUTHORIZED,
                error_codes::INVALID_SIGNATURE,
                "missing Paddle-Signature header",
            )
        })?;

    verify_paddle_signature(&paddle.webhook_secret, sig_header, &body, 300)?;

    let event: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| AppError::bad_request(format!("invalid JSON: {e}")))?;

    let event_id = event
        .get("event_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::bad_request("missing event_id"))?;
    let event_type = event
        .get("event_type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let occurred_at_str = event
        .get("occurred_at")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::bad_request("missing occurred_at"))?;
    let occurred_at = OffsetDateTime::parse(occurred_at_str, &Rfc3339)
        .map_err(|_| AppError::bad_request("invalid occurred_at RFC 3339"))?;

    let mut tx = state.db.begin().await?;

    // Transactional idempotency check on event_id.
    let existing: Option<(String,)> =
        sqlx::query_as("SELECT event_id FROM billing_events WHERE event_id = $1")
            .bind(event_id)
            .fetch_optional(&mut *tx)
            .await?;

    if existing.is_some() {
        tx.commit().await?;
        return Ok(StatusCode::OK);
    }

    let data = event.get("data");
    let ws_id = associated_workspace(&mut tx, data).await?;

    sqlx::query(
        "INSERT INTO billing_events (event_id, event_type, workspace_id, occurred_at, payload, processed_at)
         VALUES ($1, $2, $3, $4, $5, now())",
    )
    .bind(event_id)
    .bind(event_type)
    .bind(ws_id)
    .bind(occurred_at)
    .bind(&event)
    .execute(&mut *tx)
    .await?;

    // Subscription events for an associated workspace trigger a canonical
    // re-read from Paddle; the payload itself never writes entitlements.
    let sub_to_reconcile = match (ws_id, data) {
        (Some(ws), Some(data)) if event_type.starts_with("subscription.") => data
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|id| id.starts_with("sub_"))
            .map(|id| (id.to_string(), ws)),
        _ => None,
    };
    if let Some((sub, ws)) = &sub_to_reconcile {
        crate::billing_reconcile::enqueue(&mut tx, sub, *ws).await?;
    }
    // Paddle paid this checkout (it names it as the origin of a subscription):
    // never offer its payment link again.
    if let Some(txn) = data
        .and_then(|d| d.get("transaction_id"))
        .and_then(|v| v.as_str())
    {
        sqlx::query(
            "UPDATE billing_checkouts SET status = 'completed', updated_at = now()
              WHERE transaction_id = $1 AND status <> 'completed'",
        )
        .bind(txn)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    if let Some((sub, _)) = sub_to_reconcile {
        crate::billing_reconcile::run_now(&state, &sub).await;
    }
    Ok(StatusCode::OK)
}
