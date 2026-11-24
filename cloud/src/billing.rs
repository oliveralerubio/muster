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
    UpdatePlan, WorkspaceBilling, MAX_TEAM_SEATS,
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

/// Which of a workspace's subscriptions decides its entitlements: a
/// workspace can accumulate several (canceled, then subscribed again); a
/// live one wins over a canceled one, then the most recently updated by
/// Paddle. The `ORDER BY` of every query that picks it.
pub(crate) const CURRENT_SUBSCRIPTION_ORDER: &str =
    "(status <> 'canceled') DESC, provider_updated_at DESC NULLS LAST, updated_at DESC";

/// The subscription that decides a workspace's entitlements (see
/// [`CURRENT_SUBSCRIPTION_ORDER`]).
pub(crate) async fn current_subscription<'e>(
    db: impl sqlx::PgExecutor<'e>,
    ws: Uuid,
) -> Result<Option<SubRow>, AppError> {
    Ok(sqlx::query_as(&format!(
        "SELECT plan, status, seats, current_period_end, cancel_at_period_end
           FROM subscriptions WHERE workspace_id = $1
          ORDER BY {CURRENT_SUBSCRIPTION_ORDER}
          LIMIT 1"
    ))
    .bind(ws)
    .fetch_optional(db)
    .await?)
}

/// A workspace's plan status and whether it may push: from the status of its
/// current subscription when it has one ([`subscription_entitlement`]),
/// from the trial otherwise. The one rule behind `GET /billing` and the
/// workspace list and detail (the push gate applies the same rule).
pub(crate) fn plan_status(
    subscription_status: Option<&str>,
    trial_ends_at: OffsetDateTime,
    now: OffsetDateTime,
) -> (PlanStatus, bool) {
    match subscription_status {
        Some(status) => subscription_entitlement(status),
        None if now < trial_ends_at => (PlanStatus::Trialing, true),
        None => (PlanStatus::TrialExpired, false),
    }
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
/// [`current_subscription`]). They follow entitlement, not the plan name: a
/// subscription that no longer entitles (canceled, paused) gets the same
/// base limits as no subscription. Only new links and pushes are measured
/// against them; nothing already stored is removed or hidden.
fn plan_limits(sub: Option<&SubRow>) -> PlanLimits {
    match sub {
        Some((plan, status, seats, _, _)) if subscription_entitlement(status).1 => {
            if plan == "team" {
                // Seats come from Paddle's quantity: never trust them to fit.
                let seats = u32::try_from(*seats).unwrap_or(0).max(1);
                PlanLimits {
                    max_devices: seats.saturating_mul(50),
                    max_bundle_bytes: MAX_PLAN_BUNDLE_BYTES,
                    max_storage_bytes: u64::from(seats).saturating_mul(100 * GIB),
                }
            } else {
                PlanLimits {
                    max_devices: 10,
                    max_bundle_bytes: MAX_PLAN_BUNDLE_BYTES,
                    max_storage_bytes: 10 * GIB,
                }
            }
        }
        _ => PlanLimits {
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
           FROM proposals WHERE workspace_id = $1 AND status = 'open'",
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

/// The reads the service makes with the Paddle API key, by the permission
/// each needs, and a cheap authenticated call that exercises it (one-item
/// list pages; nothing is created or changed).
const KEY_READS: [(&str, &str); 2] = [
    ("subscription.read", "/subscriptions?per_page=1"),
    ("transaction.read", "/transactions?per_page=1"),
];

/// How long each startup read of the key check may take.
const KEY_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How many times a read Paddle refuses is asked before the refusal counts:
/// one refusal may be a brief incident at Paddle.
const KEY_CHECK_ATTEMPTS: u32 = 3;
/// The pause between two attempts of a refused read (about 15 s in all).
const KEY_CHECK_PAUSE: std::time::Duration = std::time::Duration::from_secs(5);

/// What a check of the Paddle API key found.
#[derive(Debug)]
pub enum KeyCheck {
    /// Paddle accepted every read the service relies on.
    Accepted,
    /// Paddle itself refused the key (`401`) or permissions of it (`403`)
    /// on every attempt: billing actions cannot work until the key is
    /// fixed. Names every missing permission, never the key or Paddle's
    /// answer.
    Refused(String),
    /// Paddle could not be asked, or the answer was not Paddle's (network,
    /// timeout, `5xx`, rate limit, a refusal from something in front of
    /// Paddle): the key is unverified, which never changes anything.
    Unverified(Vec<String>),
}

/// What one read of the key check found.
enum KeyRead {
    Accepted,
    /// Paddle answered `401` on every attempt.
    KeyRejected,
    /// Paddle answered `403` on every attempt.
    PermissionMissing,
    Unverified(String),
}

/// Whether a `401`/`403` answer is Paddle's own: Paddle's JSON error shape,
/// an `error.code` and a `meta.request_id` (an API gateway in front of it
/// may send an `error.code` of its own, never Paddle's request id). Only
/// those fields are read; neither they nor the body are kept.
async fn is_paddle_refusal(resp: reqwest::Response) -> bool {
    let present = |v: &serde_json::Value| v.as_str().is_some_and(|s| !s.is_empty());
    resp.json::<serde_json::Value>()
        .await
        .is_ok_and(|body| present(&body["error"]["code"]) && present(&body["meta"]["request_id"]))
}

/// One read of [`KEY_READS`], retried while Paddle refuses it.
async fn key_read(paddle: &crate::config::PaddleConfig, permission: &str, path: &str) -> KeyRead {
    let mut refused = KeyRead::Accepted;
    for attempt in 1..=KEY_CHECK_ATTEMPTS {
        if attempt > 1 {
            tokio::time::sleep(KEY_CHECK_PAUSE).await;
        }
        let resp = match crate::billing_reconcile::paddle_http()
            .get(format!("{}{path}", paddle.base_url))
            .bearer_auth(&paddle.api_key)
            .timeout(KEY_CHECK_TIMEOUT)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                return KeyRead::Unverified(format!(
                    "{permission}: paddle unreachable: {}",
                    e.without_url()
                ))
            }
        };
        let status = resp.status();
        if status.is_success() {
            return KeyRead::Accepted;
        }
        if status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN {
            return KeyRead::Unverified(format!("{permission}: paddle answered {status}"));
        }
        if !is_paddle_refusal(resp).await {
            return KeyRead::Unverified(format!(
                "{permission}: {status} without Paddle's error (a proxy in front of Paddle?)"
            ));
        }
        refused = if status == StatusCode::UNAUTHORIZED {
            KeyRead::KeyRejected
        } else {
            KeyRead::PermissionMissing
        };
    }
    refused
}

/// Checks once that Paddle accepts the configured API key for every read
/// the service makes ([`KEY_READS`], all of them, concurrently), so a key
/// without a needed permission is reported clearly instead of as later
/// retries and errors.
pub async fn check_api_key(paddle: &crate::config::PaddleConfig) -> KeyCheck {
    let [(sub_perm, sub_path), (txn_perm, txn_path)] = KEY_READS;
    let (subs, txns) = tokio::join!(
        key_read(paddle, sub_perm, sub_path),
        key_read(paddle, txn_perm, txn_path)
    );
    let reads = [(sub_perm, subs), (txn_perm, txns)];
    if reads
        .iter()
        .any(|(_, read)| matches!(read, KeyRead::KeyRejected))
    {
        return KeyCheck::Refused(format!(
            "Paddle rejected the API key MUSTER_PADDLE_API_KEY ({}) on {KEY_CHECK_ATTEMPTS} \
             attempts: it is invalid, revoked or from another Paddle account or environment",
            StatusCode::UNAUTHORIZED
        ));
    }
    let missing: Vec<&str> = reads
        .iter()
        .filter(|(_, read)| matches!(read, KeyRead::PermissionMissing))
        .map(|(permission, _)| *permission)
        .collect();
    if !missing.is_empty() {
        return KeyCheck::Refused(format!(
            "the API key MUSTER_PADDLE_API_KEY lacks the Paddle permission(s) {} ({} on \
             {KEY_CHECK_ATTEMPTS} attempts); grant them to the key in Paddle > Developer tools \
             > Authentication",
            missing.join(", "),
            StatusCode::FORBIDDEN
        ));
    }
    let unverified: Vec<String> = reads
        .into_iter()
        .filter_map(|(_, read)| match read {
            KeyRead::Unverified(why) => Some(why),
            _ => None,
        })
        .collect();
    if unverified.is_empty() {
        KeyCheck::Accepted
    } else {
        KeyCheck::Unverified(unverified)
    }
}

/// Why billing actions are unavailable although Paddle is configured: the
/// last key check found the API key refused by Paddle. `None` while billing
/// is available, including before the first check has a result.
static KEY_REFUSED: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

fn key_refused() -> Option<String> {
    match KEY_REFUSED.read() {
        Ok(current) => current.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Records the latest refusal (`None`: accepted); returns the previous one.
fn set_key_refused(refused: Option<String>) -> Option<String> {
    match KEY_REFUSED.write() {
        Ok(mut current) => std::mem::replace(&mut *current, refused),
        Err(poisoned) => std::mem::replace(&mut *poisoned.into_inner(), refused),
    }
}

/// The Paddle configuration billing actions (checkout, plan change,
/// cancellation, portal) may use: `503 billing_unconfigured` when billing is
/// not configured, or while Paddle refuses the API key (without calling
/// Paddle). Webhooks and reconciliation do not depend on this.
fn paddle_for_actions(state: &AppState) -> Result<&crate::config::PaddleConfig, AppError> {
    let Some(paddle) = &state.config.paddle else {
        return Err(AppError::billing_unconfigured());
    };
    match key_refused() {
        None => Ok(paddle),
        Some(why) => Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BILLING_UNCONFIGURED,
            format!(
                "billing is temporarily unavailable: {why}. It comes back by itself once \
                 Paddle accepts the key (checked every {} s)",
                state.config.paddle_key_check_every().as_secs()
            ),
        )),
    }
}

/// Background loop: checks the Paddle API key ([`check_api_key`]) right
/// away and then every `every`, only while this instance is active (a
/// fenced or candidate instance never uses the key). A refusal makes
/// billing actions unavailable, logged once as an error; acceptance makes
/// them available again by itself. An unverified check changes nothing.
pub async fn key_check_worker(state: AppState, every: std::time::Duration) {
    let Some(paddle) = state.config.paddle_key_check().cloned() else {
        return;
    };
    let mut warned_unverified = false;
    loop {
        if state
            .role_gate
            .allows(&state.config.control, &state.db, "paddle key check")
            .await
        {
            match check_api_key(&paddle).await {
                KeyCheck::Accepted => {
                    warned_unverified = false;
                    if set_key_refused(None).is_some() {
                        tracing::info!("paddle API key accepted again; billing available");
                    }
                }
                KeyCheck::Refused(why) => {
                    if set_key_refused(Some(why.clone())).as_deref() != Some(why.as_str()) {
                        tracing::error!(reason = %why, recheck_in_secs = every.as_secs(),
                            "billing unavailable: Paddle refuses the API key; sync, sign-in and \
                             webhooks keep working");
                    }
                }
                KeyCheck::Unverified(why) => {
                    if !warned_unverified {
                        tracing::warn!(reasons = ?why,
                            "paddle API key not verified (Paddle unreachable, or an answer \
                             that was not Paddle's); billing unchanged");
                        warned_unverified = true;
                    }
                }
            }
        }
        tokio::time::sleep(every).await;
    }
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
    // Storage usage reads the Git storage: hold it shared like every Git
    // access, so a purge drains this request too.
    let _access = state.repo_access(ws).await?;

    let ws_row: Option<(OffsetDateTime,)> =
        sqlx::query_as("SELECT trial_ends_at FROM workspaces WHERE id = $1")
            .bind(ws)
            .fetch_optional(&state.db)
            .await?;
    let (trial_ends_at,) = ws_row.ok_or_else(AppError::not_found)?;

    let now = OffsetDateTime::now_utc();
    // Configured and not refused by Paddle: billing actions can be used.
    let billing_configured = state.config.paddle.is_some() && key_refused().is_none();

    let sub = current_subscription(&state.db, ws).await?;
    let limits = limits_for(&state.config, sub.as_ref());
    let storage_used = storage_used(&state, &state.db, ws).await?;

    let billing = match sub {
        Some((plan, status_str, seats, period_end, cancel_at_end)) => {
            let (plan_status, can_push) = plan_status(Some(&status_str), trial_ends_at, now);
            WorkspaceBilling {
                workspace_id: ws.to_string(),
                billing_configured,
                plan,
                plan_status,
                seats: u32::try_from(seats).unwrap_or(0),
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
            let (plan_status, can_push) = plan_status(None, trial_ends_at, now);
            WorkspaceBilling {
                workspace_id: ws.to_string(),
                billing_configured,
                plan: "trial".into(),
                plan_status,
                seats: 1,
                trial_ends_at: Some(rfc3339(trial_ends_at)?),
                current_period_end: None,
                cancel_at_period_end: false,
                can_push,
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

/// How long a billing action waits for the one already running on the same
/// workspace before answering `503 not_ready` (retry later).
const BILLING_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Begins the transaction of a billing action of workspace `ws` (checkout,
/// plan change, cancellation), holding the workspace's billing lock until it
/// ends: one decision, and one Paddle call, at a time per workspace, across
/// processes. It is not the workspace row lock, so holding it across a
/// Paddle call (up to its 15 s timeout) never holds up a push, a link or a
/// deletion request.
///
/// Only the action holding the lock holds a database connection. Another
/// one waits without a connection (each try is a short transaction that
/// ends at once when the lock is taken) for at most [`BILLING_LOCK_WAIT`],
/// so actions piling up behind a slow Paddle never exhaust the pool.
async fn begin_billing_action(
    state: &AppState,
    ws: Uuid,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, AppError> {
    let deadline = tokio::time::Instant::now() + BILLING_LOCK_WAIT;
    let mut pause = std::time::Duration::from_millis(20);
    loop {
        let mut tx = state.db.begin().await?;
        let locked: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_xact_lock(hashtext('muster.billing'), hashtext($1::text))",
        )
        .bind(ws.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if locked {
            return Ok(tx);
        }
        tx.rollback().await?;
        if tokio::time::Instant::now() + pause > deadline {
            return Err(AppError::not_ready(
                "another billing action for this workspace is in progress; retry shortly",
            ));
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(std::time::Duration::from_millis(250));
    }
}

/// `404` for a missing workspace and `409 workspace_deletion_scheduled`
/// after a deletion request, read without locking the workspace row. A
/// deletion that lands later is caught by the short re-check after the
/// Paddle call.
async fn ensure_writable_unlocked(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ws: Uuid,
) -> Result<(), AppError> {
    sqlx::query("SELECT id FROM workspaces WHERE id = $1")
        .bind(ws)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(AppError::not_found)?;
    crate::lifecycle::deny_if_scheduled(tx, ws).await
}

/// The seat count a checkout or plan change asks Paddle for: one for a
/// personal plan, at least one for a team, and never above
/// [`MAX_TEAM_SEATS`] (`400`, `details.field = "seats"`), so it always fits
/// the stored `INT` and the limit arithmetic.
fn requested_seats(is_team: bool, seats: u32) -> Result<i32, AppError> {
    if seats > MAX_TEAM_SEATS {
        return Err(
            AppError::bad_request(format!("seats must be at most {MAX_TEAM_SEATS}"))
                .with_details(serde_json::json!({ "field": "seats" })),
        );
    }
    let seats = if is_team { seats.max(1) } else { 1 };
    i32::try_from(seats).map_err(AppError::internal)
}

/// `POST /v1/workspaces/{ws}/billing/checkout`: creates a Paddle checkout
/// transaction associated with this workspace.
///
/// The decision runs to completion even when the client gives up (a closed
/// connection drops this handler), so a Paddle transaction it creates is
/// always recorded; it is bounded by [`CHECKOUT_DEADLINE`] and holds one
/// database connection, only while it holds the workspace's billing lock.
pub async fn create_checkout(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<CheckoutSession>, AppError> {
    account.require_scope(scopes::WORKSPACES_WRITE)?;
    let ws = parse_uuid(&ws_str)?;
    require_admin_or_owner(&state, ws, account.user_id).await?;

    paddle_for_actions(&state)?;

    let req: CreateCheckout = parse_json(&body)?;
    let is_team = req.plan == "team";
    let seats = requested_seats(is_team, req.seats)?;
    let user_id = account.user_id;
    tokio::spawn(async move {
        let decision = checkout_decision(&state, ws, user_id, is_team, seats);
        tokio::time::timeout(CHECKOUT_DEADLINE, decision)
            .await
            .unwrap_or_else(|_| {
                Err(AppError::not_ready(
                    "the billing provider did not answer in time; retry shortly",
                ))
            })
    })
    .await
    .map_err(AppError::internal)?
    .map(Json)
}

/// The longest a checkout decision may take: its lock wait plus the Paddle
/// calls it can make (each with a 15 s timeout). When it is reached the
/// decision is abandoned and its transaction rolled back; what Paddle did
/// meanwhile is read back from Paddle by the next request.
const CHECKOUT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// How many times in a row the stored link of an open checkout is offered
/// while Paddle cannot tell its status (each time logged). After that the
/// checkout answers `503 not_ready` until Paddle answers again, which also
/// starts the count over: a link Paddle may have canceled is never handed
/// out indefinitely.
const MAX_UNVERIFIED_OFFERS: i32 = 3;

/// What Paddle reports about a checkout this server offered.
enum CheckoutAtPaddle {
    /// Still payable.
    Payable,
    /// Canceled: never to be offered again.
    Canceled,
    /// Paid (captured, then completed or billed): its subscription is on
    /// the way, settled as the deletion's checkout cancellation reads it.
    Paid,
}

fn checkout_at_paddle(status: &str) -> CheckoutAtPaddle {
    match status {
        "canceled" => CheckoutAtPaddle::Canceled,
        "paid" | "completed" | "billed" => CheckoutAtPaddle::Paid,
        _ => CheckoutAtPaddle::Payable,
    }
}

/// A transaction as Paddle holds it (`GET /transactions/{id}`).
struct PaddleTransaction {
    status: String,
    /// The subscription Paddle created from it, once it has.
    subscription_id: Option<String>,
}

/// Reads transaction `txn_id` from Paddle; the error says why it could not
/// (never the request URL or the key).
async fn read_transaction(
    paddle: &crate::config::PaddleConfig,
    txn_id: &str,
) -> Result<PaddleTransaction, String> {
    let resp = crate::billing_reconcile::paddle_http()
        .get(format!("{}/transactions/{txn_id}", paddle.base_url))
        .bearer_auth(&paddle.api_key)
        .send()
        .await
        .map_err(|e| format!("paddle unreachable: {}", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(format!("paddle answered {}", resp.status()));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid paddle json: {}", e.without_url()))?;
    let data = &body["data"];
    if data["id"].as_str() != Some(txn_id) {
        return Err("paddle returned a different transaction".into());
    }
    let status = data["status"]
        .as_str()
        .ok_or("paddle transaction without status")?
        .to_string();
    let subscription_id = data["subscription_id"]
        .as_str()
        .filter(|id| id.starts_with("sub_"))
        .map(str::to_string);
    Ok(PaddleTransaction {
        status,
        subscription_id,
    })
}

/// Records that Paddle reports checkout `txn_id` paid, with the subscription
/// it created from it when Paddle names it. The checkout becomes `paid`,
/// which refuses every other checkout of its workspace, until that
/// subscription is reconciled here (`completed`); it is `completed` at once
/// when the subscription is already registered for the workspace.
pub(crate) async fn record_paid_checkout(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    txn_id: &str,
    subscription_id: Option<&str>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE billing_checkouts
            SET status = CASE WHEN EXISTS (
                                 SELECT 1 FROM subscriptions s
                                  WHERE s.paddle_subscription_id = $2
                                    AND s.workspace_id = billing_checkouts.workspace_id)
                              THEN 'completed' ELSE 'paid' END,
                subscription_id = COALESCE($2, subscription_id),
                updated_at = now()
          WHERE transaction_id = $1 AND status IN ('open', 'paid')",
    )
    .bind(txn_id)
    .bind(subscription_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn paid_checkout_conflict() -> AppError {
    AppError::new(
        StatusCode::CONFLICT,
        error_codes::SUBSCRIPTION_EXISTS,
        "a checkout of this workspace was paid; its subscription is being set up",
    )
}

/// The open checkout `txn_id` turned out paid: recorded (blocking other
/// checkouts), its subscription queued and reconciled now when Paddle names
/// it, and the request refused.
async fn refuse_as_paid(
    state: &AppState,
    mut tx: sqlx::Transaction<'static, sqlx::Postgres>,
    ws: Uuid,
    txn_id: &str,
    subscription_id: Option<String>,
) -> Result<CheckoutSession, AppError> {
    record_paid_checkout(&mut tx, txn_id, subscription_id.as_deref()).await?;
    if let Some(sub) = &subscription_id {
        crate::billing_reconcile::enqueue(&mut tx, sub, ws).await?;
    }
    tx.commit().await?;
    if let Some(sub) = &subscription_id {
        crate::billing_reconcile::run_now(state, sub).await;
    }
    Err(paid_checkout_conflict())
}

async fn mark_checkout(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    txn_id: &str,
    status: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE billing_checkouts SET status = $2, updated_at = now()
          WHERE transaction_id = $1 AND status = 'open'",
    )
    .bind(txn_id)
    .bind(status)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn checkout_decision(
    state: &AppState,
    ws: Uuid,
    user_id: Uuid,
    is_team: bool,
    seats: i32,
) -> Result<CheckoutSession, AppError> {
    let Some(paddle) = &state.config.paddle else {
        return Err(AppError::billing_unconfigured());
    };
    let plan = if is_team { "team" } else { "personal" };
    let price_id = if is_team {
        &paddle.price_team_seat
    } else {
        &paddle.price_personal
    };

    // One checkout decision at a time per workspace: concurrent requests
    // and retries see the same open checkout instead of each creating a
    // payable Paddle transaction.
    let mut tx = begin_billing_action(state, ws).await?;
    ensure_writable_unlocked(&mut tx, ws).await?;
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

    // A paid checkout whose subscription is not registered yet (its webhook
    // late or lost): no second checkout, whatever the plan. Its
    // subscription is reconciled from Paddle now instead of waiting.
    let paid: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT transaction_id, subscription_id FROM billing_checkouts
          WHERE workspace_id = $1 AND status = 'paid'",
    )
    .bind(ws)
    .fetch_all(&mut *tx)
    .await?;
    if !paid.is_empty() {
        let mut reconcile = Vec::new();
        for (txn_id, known) in paid {
            let sub = match known {
                Some(sub) => Some(sub),
                None => match read_transaction(paddle, &txn_id).await {
                    Ok(t) => t.subscription_id,
                    Err(why) => {
                        tracing::warn!(workspace = %ws, transaction = %txn_id, reason = %why,
                            "billing checkout: could not read a paid checkout's subscription");
                        None
                    }
                },
            };
            if let Some(sub) = sub {
                record_paid_checkout(&mut tx, &txn_id, Some(&sub)).await?;
                crate::billing_reconcile::enqueue(&mut tx, &sub, ws).await?;
                reconcile.push(sub);
            }
        }
        tx.commit().await?;
        for sub in reconcile {
            crate::billing_reconcile::run_now(state, &sub).await;
        }
        return Err(paid_checkout_conflict());
    }

    type OpenCheckout = (String, String, i32, Option<String>, i32);
    let open: Option<OpenCheckout> = sqlx::query_as(
        "SELECT transaction_id, plan, seats, checkout_url, unverified_offers
           FROM billing_checkouts
          WHERE workspace_id = $1 AND status = 'open'",
    )
    .bind(ws)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((txn_id, open_plan, open_seats, url, unverified)) = open {
        let reusable = url.filter(|_| open_plan == plan && open_seats == seats);
        if let Some(url) = reusable {
            // Offered again only while Paddle still reports it payable: a
            // cancel whose answer was lost, or one made at Paddle, is caught
            // here instead of handing out a dead link.
            match read_transaction(paddle, &txn_id).await {
                Ok(t) => match checkout_at_paddle(&t.status) {
                    CheckoutAtPaddle::Payable => {
                        if unverified > 0 {
                            sqlx::query(
                                "UPDATE billing_checkouts SET unverified_offers = 0
                                  WHERE transaction_id = $1",
                            )
                            .bind(&txn_id)
                            .execute(&mut *tx)
                            .await?;
                        }
                        tx.commit().await?;
                        return Ok(CheckoutSession {
                            checkout_url: url,
                            transaction_id: txn_id,
                        });
                    }
                    CheckoutAtPaddle::Canceled => {
                        mark_checkout(&mut tx, &txn_id, "canceled").await?
                    }
                    CheckoutAtPaddle::Paid => {
                        return refuse_as_paid(state, tx, ws, &txn_id, t.subscription_id).await
                    }
                },
                Err(why) if unverified >= MAX_UNVERIFIED_OFFERS => {
                    tracing::warn!(workspace = %ws, transaction = %txn_id, reason = %why,
                        offers = unverified,
                        "billing checkout: Paddle still cannot confirm the open checkout; not offering it");
                    return Err(AppError::not_ready(
                        "Paddle cannot confirm the open checkout right now; retry later",
                    ));
                }
                Err(why) => {
                    // Unreadable now (Paddle down, or a key without permission
                    // to read transactions): the stored link is all there is,
                    // offered a bounded number of times.
                    tracing::warn!(workspace = %ws, transaction = %txn_id, reason = %why,
                        offer = unverified + 1, max_offers = MAX_UNVERIFIED_OFFERS,
                        "billing checkout: Paddle could not confirm the open checkout; offering its stored link");
                    sqlx::query(
                        "UPDATE billing_checkouts SET unverified_offers = unverified_offers + 1
                          WHERE transaction_id = $1",
                    )
                    .bind(&txn_id)
                    .execute(&mut *tx)
                    .await?;
                    tx.commit().await?;
                    return Ok(CheckoutSession {
                        checkout_url: url,
                        transaction_id: txn_id,
                    });
                }
            }
        } else {
            // A different plan: the old payment link must stop being payable
            // before a new one exists (Paddle: PATCH status=canceled, alone).
            let cancel = crate::billing_reconcile::paddle_http()
                .patch(format!("{}/transactions/{txn_id}", paddle.base_url))
                .bearer_auth(&paddle.api_key)
                .json(&serde_json::json!({ "status": "canceled" }))
                .send()
                .await;
            let at_paddle = match &cancel {
                Ok(resp) if resp.status().is_success() => Ok(PaddleTransaction {
                    status: "canceled".into(),
                    subscription_id: None,
                }),
                // Refused (Paddle does not name why: already canceled, or
                // paid) or unanswered: the transaction says what happened.
                _ => read_transaction(paddle, &txn_id).await,
            };
            match at_paddle {
                Ok(t) => match checkout_at_paddle(&t.status) {
                    // Settled in this transaction: if it is rolled back (the
                    // new checkout fails), the next request reads Paddle's
                    // canceled status again before offering anything.
                    CheckoutAtPaddle::Canceled => {
                        mark_checkout(&mut tx, &txn_id, "canceled").await?
                    }
                    CheckoutAtPaddle::Paid => {
                        return refuse_as_paid(state, tx, ws, &txn_id, t.subscription_id).await
                    }
                    CheckoutAtPaddle::Payable => {
                        return Err(AppError::internal(
                            "paddle did not cancel the open checkout; it is still payable",
                        ))
                    }
                },
                Err(why) => {
                    let cancel = match cancel {
                        Ok(resp) => format!("refused with {}", resp.status()),
                        Err(e) => format!("error: {}", e.without_url()),
                    };
                    return Err(AppError::internal(format!(
                        "paddle transaction cancel {cancel}; status unreadable: {why}"
                    )));
                }
            }
        }
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
            "user_id": user_id.to_string(),
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

    // Short re-check under the workspace row lock, serialised with a
    // deletion request: either the request comes after this commit and
    // journals this checkout to cancel it, or it came during the Paddle call
    // and the new transaction is handed to its provider cancellation now.
    if let Err(denied) = crate::lifecycle::lock_writable(&mut tx, ws, true).await {
        drop(tx);
        if denied.code == error_codes::WORKSPACE_DELETION_SCHEDULED {
            crate::lifecycle_provider::cancel_late_checkout(state, ws, &txn_id).await;
        }
        return Err(denied);
    }
    // The association that lets `subscription.created` grant rights.
    sqlx::query(
        "INSERT INTO billing_checkouts
                (transaction_id, workspace_id, created_by, plan, seats, price_id, status, checkout_url)
         VALUES ($1, $2, $3, $4, $5, $6, 'open', $7)",
    )
    .bind(&txn_id)
    .bind(ws)
    .bind(user_id)
    .bind(plan)
    .bind(seats)
    .bind(price_id)
    .bind(&checkout_url)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(CheckoutSession {
        checkout_url,
        transaction_id: txn_id,
    })
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

    let paddle = paddle_for_actions(&state)?;

    // Refused after a deletion request like every other billing action: the
    // provider objects are being canceled and nothing is left to manage.
    let cust_row: Option<(String,)> = {
        let mut tx = state.db.begin().await?;
        ensure_writable_unlocked(&mut tx, ws).await?;
        let row =
            sqlx::query_as("SELECT customer_id FROM billing_customers WHERE workspace_id = $1")
                .bind(ws)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        row
    };

    let (cust_id,) = cust_row
        .ok_or_else(|| AppError::bad_request("workspace has no billing customer associated yet"))?;

    // The shared provider client: timeouts, and no redirects (the API key
    // must never follow one).
    let resp = crate::billing_reconcile::paddle_http()
        .post(format!(
            "{}/customers/{cust_id}/portal-sessions",
            paddle.base_url
        ))
        .bearer_auth(&paddle.api_key)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| AppError::internal(format!("paddle portal error: {}", e.without_url())))?;

    if !resp.status().is_success() {
        return Err(AppError::internal(format!(
            "paddle portal rejected with {}",
            resp.status()
        )));
    }

    let resp_json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::internal(format!("invalid paddle json: {}", e.without_url())))?;

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

    let paddle = paddle_for_actions(&state)?;

    // Billing actions of the workspace are serialised (concurrent cancels
    // ask Paddle once) without holding the workspace row during the call.
    // After a deletion request the provider work cancels immediately.
    let mut tx = begin_billing_action(&state, ws).await?;
    ensure_writable_unlocked(&mut tx, ws).await?;
    let sub_row: Option<(String, String, bool)> = sqlx::query_as(&format!(
        "SELECT paddle_subscription_id, status, cancel_at_period_end FROM subscriptions
          WHERE workspace_id = $1
          ORDER BY {CURRENT_SUBSCRIPTION_ORDER}
          LIMIT 1"
    ))
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
    // Paddle has made the change, so it is re-read even if a deletion
    // request landed during the call (the reconciliation then hands a live
    // subscription to its immediate cancellation).
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

    let paddle = paddle_for_actions(&state)?;

    let req: UpdatePlan = parse_json(&body)?;
    let is_team = req.plan == "team";
    let seats = requested_seats(is_team, req.seats)?;
    let price_id = if is_team {
        &paddle.price_team_seat
    } else {
        &paddle.price_personal
    };

    // Serialised with the other billing actions of the workspace, without
    // holding the workspace row during the Paddle call; refused after a
    // deletion request.
    let mut tx = begin_billing_action(&state, ws).await?;
    ensure_writable_unlocked(&mut tx, ws).await?;
    let sub_row: Option<(String,)> = sqlx::query_as(
        "SELECT paddle_subscription_id FROM subscriptions
          WHERE workspace_id = $1 AND status <> 'canceled'
          ORDER BY provider_updated_at DESC NULLS LAST, updated_at DESC
          LIMIT 1",
    )
    .bind(ws)
    .fetch_optional(&mut *tx)
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

    // Entitlements follow Paddle's canonical subscription, re-read even if a
    // deletion request landed during the call (as for a cancellation).
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

/// Resolves the workspace from a recorded checkout or subscription. A completed
/// transaction identifies its checkout by `id`; subscription events can name
/// their originating checkout by `transaction_id`. Hold the workspace row so
/// its tombstone cannot commit between this lookup and recording the event.
async fn associated_workspace(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event_type: &str,
    data: Option<&serde_json::Value>,
) -> Result<Option<Uuid>, AppError> {
    let Some(data) = data else { return Ok(None) };
    let is_completed_transaction = event_type == "transaction.completed";
    if let Some(sub_id) = data["id"].as_str().filter(|_| !is_completed_transaction) {
        let bound = sqlx::query_scalar(
            "SELECT w.id FROM subscriptions s JOIN workspaces w ON w.id = s.workspace_id
              WHERE s.paddle_subscription_id = $1 FOR KEY SHARE OF w",
        )
        .bind(sub_id)
        .fetch_optional(&mut **tx)
        .await?;
        if bound.is_some() {
            return Ok(bound);
        }
    }
    let checkout_field = if is_completed_transaction {
        "id"
    } else {
        "transaction_id"
    };
    let Some(txn_id) = data[checkout_field].as_str() else {
        return Ok(None);
    };
    Ok(sqlx::query_scalar(
        "SELECT w.id FROM billing_checkouts c JOIN workspaces w ON w.id = c.workspace_id
          WHERE c.transaction_id = $1 FOR KEY SHARE OF w",
    )
    .bind(txn_id)
    .fetch_optional(&mut **tx)
    .await?)
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

    let data = event.get("data");
    let ws_id = associated_workspace(&mut tx, event_type, data).await?;
    // An event about a tombstoned workspace (associated with it, naming it in
    // `custom_data`, or about one of its provider objects) is kept for dedup
    // with an empty payload.
    let ids: Vec<String> = ["id", "transaction_id", "subscription_id"]
        .iter()
        .filter_map(|k| data.and_then(|d| d.get(*k)).and_then(|v| v.as_str()))
        .map(str::to_string)
        .collect();
    let tombstoned: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM workspace_lifecycle l
                         WHERE l.purge_committed_at IS NOT NULL
                           AND (l.workspace_id = $1
                                OR l.workspace_id::text = $3
                                OR EXISTS (SELECT 1 FROM lifecycle_provider_items i
                                            WHERE i.workspace_id = l.workspace_id
                                              AND i.provider_id = ANY($2))
                                OR EXISTS (SELECT 1 FROM lifecycle_purged_provider_ids p
                                            WHERE p.workspace_id = l.workspace_id
                                              AND p.provider_id = ANY($2))))",
    )
    .bind(ws_id)
    .bind(&ids)
    .bind(
        data.and_then(|d| d.pointer("/custom_data/workspace_id"))
            .and_then(|v| v.as_str()),
    )
    .fetch_one(&mut *tx)
    .await?;
    let stored = if tombstoned {
        serde_json::json!({})
    } else {
        event.clone()
    };
    let ws_id = ws_id.filter(|_| !tombstoned);

    // Dedupe on event_id by the insert itself: a concurrent delivery of the
    // same event waits for this one and then inserts nothing. A duplicate
    // is acknowledged (Paddle stops retrying) and changes nothing.
    let inserted = sqlx::query(
        "INSERT INTO billing_events (event_id, event_type, workspace_id, occurred_at, payload, processed_at)
         VALUES ($1, $2, $3, $4, $5, now())
         ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(event_id)
    .bind(event_type)
    .bind(ws_id)
    .bind(occurred_at)
    .bind(&stored)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        tx.rollback().await?;
        return Ok(StatusCode::OK);
    }

    let (Some(ws), Some(data)) = (ws_id, data) else {
        tx.commit().await?;
        return Ok(StatusCode::OK);
    };

    // Only these events report payment or subscription changes. Record the
    // paid checkout before queueing so reconciliation can see its origin.
    let (paid_txn, sub) = match event_type {
        "transaction.completed" => (data["id"].as_str(), data["subscription_id"].as_str()),
        kind if kind.starts_with("subscription.") => {
            (data["transaction_id"].as_str(), data["id"].as_str())
        }
        _ => (None, None),
    };
    let sub = sub.filter(|id| id.starts_with("sub_"));
    if let Some(txn) = paid_txn {
        record_paid_checkout(&mut tx, txn, sub).await?;
    }
    // Entitlements come from the canonical subscription at Paddle.
    if let Some(sub) = sub {
        crate::billing_reconcile::enqueue(&mut tx, sub, ws).await?;
    }
    tx.commit().await?;

    if let Some(sub) = sub {
        crate::billing_reconcile::run_now(&state, sub).await;
    }
    Ok(StatusCode::OK)
}
