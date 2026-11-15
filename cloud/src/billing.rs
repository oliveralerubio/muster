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

    type SubRow = (String, String, i32, OffsetDateTime, bool);
    let sub: Option<SubRow> = sqlx::query_as(
        "SELECT plan, status, seats, current_period_end, cancel_at_period_end
         FROM subscriptions WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_optional(&state.db)
    .await?;

    let billing = match sub {
        Some((plan, status_str, seats, period_end, cancel_at_end)) => {
            let plan_status = match status_str.as_str() {
                "active" => PlanStatus::Active,
                "past_due" => PlanStatus::PastDue,
                "canceled" => PlanStatus::Canceled,
                _ => PlanStatus::Active,
            };
            let can_push = matches!(plan_status, PlanStatus::Active | PlanStatus::PastDue);
            let (max_devices, max_storage) = if plan == "team" {
                (
                    (seats as u32) * 50,
                    (seats as u64) * 100 * 1024 * 1024 * 1024,
                )
            } else {
                (10, 10 * 1024 * 1024 * 1024)
            };
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
                max_devices,
                max_bundle_bytes: 25 * 1024 * 1024,
                max_storage_bytes: max_storage,
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
                max_devices: 3,
                max_bundle_bytes: 10 * 1024 * 1024,
                max_storage_bytes: 1024 * 1024 * 1024,
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
    let seats = if is_team { req.seats.max(1) } else { 1 };
    let price_id = if is_team {
        &paddle.price_team_seat
    } else {
        &paddle.price_personal
    };

    let customer_row: Option<(String,)> =
        sqlx::query_as("SELECT customer_id FROM billing_customers WHERE workspace_id = $1")
            .bind(ws)
            .fetch_optional(&state.db)
            .await?;

    let client = reqwest::Client::new();
    let url = format!("{}/transactions", paddle.base_url);

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
            "plan": req.plan
        }
    });

    if let Some((cust_id,)) = customer_row {
        payload["customer_id"] = serde_json::Value::String(cust_id);
    }

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", paddle.api_key))
        .json(&payload)
        .send()
        .await
        .map_err(|e| AppError::internal(format!("paddle transactions api error: {e}")))?;

    if !resp.status().is_success() {
        let err_text = resp.text().await.unwrap_or_default();
        return Err(AppError::internal(format!(
            "paddle rejected transaction: {err_text}"
        )));
    }

    let resp_json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::internal(format!("invalid paddle json: {e}")))?;

    let data = resp_json
        .get("data")
        .ok_or_else(|| AppError::internal("no data in paddle resp"))?;
    let txn_id = data
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let checkout_url = data
        .get("checkout")
        .and_then(|c| c.get("url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| AppError::internal("no checkout url in paddle resp"))?
        .to_string();

    Ok(Json(CheckoutSession {
        checkout_url,
        transaction_id: txn_id,
    }))
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

    let sub_row: Option<(String,)> =
        sqlx::query_as("SELECT paddle_subscription_id FROM subscriptions WHERE workspace_id = $1")
            .bind(ws)
            .fetch_optional(&state.db)
            .await?;

    let (sub_id,) = sub_row
        .ok_or_else(|| AppError::bad_request("workspace has no active subscription to cancel"))?;

    let client = reqwest::Client::new();
    let url = format!("{}/subscriptions/{sub_id}/cancel", paddle.base_url);

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", paddle.api_key))
        .json(&serde_json::json!({ "effective_from": "next_billing_period" }))
        .send()
        .await
        .map_err(|e| AppError::internal(format!("paddle cancel error: {e}")))?;

    if !resp.status().is_success() {
        let err_text = resp.text().await.unwrap_or_default();
        return Err(AppError::internal(format!(
            "paddle cancel rejected: {err_text}"
        )));
    }

    sqlx::query("UPDATE subscriptions SET cancel_at_period_end = true WHERE workspace_id = $1")
        .bind(ws)
        .execute(&state.db)
        .await?;

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

    let sub_row: Option<(String,)> =
        sqlx::query_as("SELECT paddle_subscription_id FROM subscriptions WHERE workspace_id = $1")
            .bind(ws)
            .fetch_optional(&state.db)
            .await?;

    let (sub_id,) = sub_row
        .ok_or_else(|| AppError::bad_request("workspace has no active subscription to update"))?;

    let client = reqwest::Client::new();
    let url = format!("{}/subscriptions/{sub_id}", paddle.base_url);

    let resp = client
        .patch(&url)
        .header("Authorization", format!("Bearer {}", paddle.api_key))
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
        .map_err(|e| AppError::internal(format!("paddle update error: {e}")))?;

    if !resp.status().is_success() {
        let err_text = resp.text().await.unwrap_or_default();
        return Err(AppError::internal(format!(
            "paddle update rejected: {err_text}"
        )));
    }

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
    let ws_id = data
        .and_then(|d| d.get("custom_data"))
        .and_then(|c| c.get("workspace_id"))
        .and_then(|w| w.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());

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

    // If the event maps to a known workspace, reconcile its subscription state.
    if let (Some(ws), Some(data)) = (ws_id, data) {
        let ws_exists: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM workspaces WHERE id = $1")
            .bind(ws)
            .fetch_optional(&mut *tx)
            .await?;

        if ws_exists.is_some() {
            let sub_id = data.get("id").and_then(|v| v.as_str());
            let customer_id = data.get("customer_id").and_then(|v| v.as_str());
            let status = data
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("active");
            let plan = data
                .get("custom_data")
                .and_then(|c| c.get("plan"))
                .and_then(|p| p.as_str())
                .unwrap_or("personal");

            let period_start = data
                .get("current_billing_period")
                .and_then(|p| p.get("starts_at"))
                .and_then(|s| s.as_str())
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                .unwrap_or(occurred_at);

            let period_end = data
                .get("current_billing_period")
                .and_then(|p| p.get("ends_at"))
                .and_then(|s| s.as_str())
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                .unwrap_or_else(|| occurred_at + time::Duration::days(30));

            let seats = data
                .get("items")
                .and_then(|i| i.as_array())
                .and_then(|arr| arr.first())
                .and_then(|item| item.get("quantity"))
                .and_then(|q| q.as_u64())
                .unwrap_or(1) as i32;

            if let Some(cust_id) = customer_id {
                sqlx::query(
                    "INSERT INTO billing_customers (workspace_id, customer_id, email, created_at)
                     VALUES ($1, $2, '', now())
                     ON CONFLICT (workspace_id) DO UPDATE SET customer_id = EXCLUDED.customer_id",
                )
                .bind(ws)
                .bind(cust_id)
                .execute(&mut *tx)
                .await?;
            }

            if let Some(sub) = sub_id {
                // Out-of-order check: do not overwrite with older event.
                let prev: Option<(OffsetDateTime,)> = sqlx::query_as(
                    "SELECT last_event_occurred_at FROM subscriptions WHERE paddle_subscription_id = $1",
                )
                .bind(sub)
                .fetch_optional(&mut *tx)
                .await?;

                let should_update = match prev {
                    Some((prev_occ,)) => occurred_at >= prev_occ,
                    None => true,
                };

                if should_update {
                    let is_canceled = status == "canceled";
                    sqlx::query(
                        "INSERT INTO subscriptions (workspace_id, paddle_subscription_id, customer_id, plan, seats, status, current_period_start, current_period_end, cancel_at_period_end, canceled_at, last_event_occurred_at, created_at, updated_at)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, false, $9, $10, now(), now())
                         ON CONFLICT (paddle_subscription_id) DO UPDATE SET
                           status = EXCLUDED.status,
                           plan = EXCLUDED.plan,
                           seats = EXCLUDED.seats,
                           current_period_start = EXCLUDED.current_period_start,
                           current_period_end = EXCLUDED.current_period_end,
                           canceled_at = EXCLUDED.canceled_at,
                           last_event_occurred_at = EXCLUDED.last_event_occurred_at,
                           updated_at = now()",
                    )
                    .bind(ws)
                    .bind(sub)
                    .bind(customer_id.unwrap_or_default())
                    .bind(plan)
                    .bind(seats)
                    .bind(status)
                    .bind(period_start)
                    .bind(period_end)
                    .bind(if is_canceled { Some(occurred_at) } else { None })
                    .bind(occurred_at)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
    }

    tx.commit().await?;

    Ok(StatusCode::OK)
}
