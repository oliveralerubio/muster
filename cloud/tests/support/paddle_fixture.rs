//! Synthetic external Paddle Billing provider fixture for integration tests.
//!
//! Runs a real HTTP server on a loopback port and keeps provider-side state:
//! transactions created by the service, the canonical subscription entities
//! (`GET /subscriptions/{id}`), and a call log. Tests drive what a customer
//! does at Paddle (`complete_checkout`, `update_subscription`) and get the
//! signed webhook Paddle would send. Endpoints follow the Paddle Billing API
//! shapes the service uses: `GET`/`POST /transactions`,
//! `GET`/`PATCH /transactions/{id}`, `POST /customers/{id}/portal-sessions`,
//! `GET /subscriptions`, `GET`/`PATCH /subscriptions/{id}`,
//! `POST /subscriptions/{id}/cancel`. Read permissions of the API key can be
//! revoked ([`PaddleFixture::revoke_permission`]): Paddle then answers `403`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub const API_KEY: &str =
    "pdl_sdbx_apikey_01gtgztp8f4kek3yd4g1wrksa3_test1234567890123456789012_abc";
pub const WEBHOOK_SECRET: &str =
    "pdl_ntfset_01gkpjp8bkm3tm53kdgkx6sms7_secret123456789012345678901234";
pub const PRICE_PERSONAL: &str = "pri_01h8vzp8f4kek3yd4g1wrksa3p";
pub const PRICE_TEAM_SEAT: &str = "pri_01h8vzp8f4kek3yd4g1wrksa3t";

type HmacSha256 = Hmac<Sha256>;
type Shared = Arc<Mutex<Inner>>;

#[derive(Default)]
struct Inner {
    /// Transaction entities (`data`) by id.
    transactions: HashMap<String, serde_json::Value>,
    /// Canonical subscription entities (`data`) by id.
    subscriptions: HashMap<String, serde_json::Value>,
    /// Calls received, by endpoint name (`get_subscription`, `cancel`, ...).
    calls: HashMap<&'static str, usize>,
    /// Upcoming `GET /subscriptions/{id}` calls that answer 503.
    failing_gets: usize,
    /// How the upcoming `POST /transactions` calls fail, in order.
    create_failures: std::collections::VecDeque<CreateFailure>,
    /// API key permissions revoked (`transaction.read`, `subscription.read`).
    revoked: HashMap<&'static str, Revocation>,
}

/// How reads covered by a revoked permission are refused.
#[derive(Clone, Copy)]
struct Revocation {
    /// How many more reads are refused; `None`: all of them.
    remaining: Option<usize>,
    /// Refused by Paddle (its JSON error) or by something in front of it (a
    /// CDN or WAF page without Paddle's error).
    by_paddle: bool,
}

/// A way `POST /transactions` can fail, injected with
/// [`PaddleFixture::fail_next_create`].
#[derive(Clone, Copy, Debug)]
pub enum CreateFailure {
    /// `503` and nothing created.
    Unavailable,
    /// `201` with a transaction but no `checkout.url` (Paddle omits it when
    /// no default payment link is set).
    WithoutCheckoutUrl,
    /// The transaction is created but the answer comes only after `delay`
    /// (longer than the service's client timeout): the answer is lost.
    Hang(std::time::Duration),
}

/// Fields of a synthetic `subscription.*` webhook event.
#[derive(Clone, Copy)]
pub struct SubscriptionEvent<'a> {
    pub event_id: &'a str,
    pub event_type: &'a str,
    pub sub_id: &'a str,
    pub customer_id: &'a str,
    pub ws_id: &'a str,
    pub plan: &'a str,
    pub seats: u32,
    pub status: &'a str,
    pub occurred_at: &'a str,
    pub period_start: &'a str,
    pub period_end: &'a str,
    /// `transaction_id` is only present on `subscription.created`.
    pub transaction_id: Option<&'a str>,
}

#[derive(Clone)]
pub struct PaddleFixture {
    pub base_url: String,
    inner: Shared,
    _task: Arc<tokio::task::JoinHandle<()>>,
}

pub fn now_rfc3339() -> String {
    rfc3339(OffsetDateTime::now_utc())
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.replace_nanosecond(0).unwrap().format(&Rfc3339).unwrap()
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

impl PaddleFixture {
    /// Generates a valid `Paddle-Signature` header for `body` at timestamp `ts`.
    pub fn sign_webhook(&self, body: &[u8], ts: i64) -> String {
        sign_payload(WEBHOOK_SECRET, body, ts)
    }

    /// A hand-built `subscription.*` payload that does not touch provider state.
    pub fn subscription_event(&self, e: &SubscriptionEvent<'_>) -> serde_json::Value {
        let SubscriptionEvent {
            event_id,
            event_type,
            sub_id,
            customer_id,
            ws_id,
            plan,
            seats,
            status,
            occurred_at,
            period_start,
            period_end,
            transaction_id,
        } = *e;
        let price_id = if plan == "team" {
            PRICE_TEAM_SEAT
        } else {
            PRICE_PERSONAL
        };
        serde_json::json!({
            "event_id": event_id,
            "event_type": event_type,
            "occurred_at": occurred_at,
            "data": {
                "id": sub_id,
                "customer_id": customer_id,
                "status": status,
                "custom_data": {
                    "workspace_id": ws_id,
                    "plan": plan
                },
                "items": [
                    {
                        "price": { "id": price_id },
                        "quantity": seats
                    }
                ],
                "current_billing_period": {
                    "starts_at": period_start,
                    "ends_at": period_end
                },
                "scheduled_change": null,
                "transaction_id": transaction_id
            }
        })
    }

    /// The customer pays the checkout transaction `txn_id`: Paddle creates
    /// subscription `sub_id` for `customer_id` from the transaction items and
    /// sends `subscription.created`. Returns that event.
    pub fn complete_checkout(
        &self,
        txn_id: &str,
        sub_id: &str,
        customer_id: &str,
    ) -> serde_json::Value {
        let mut g = self.inner.lock().unwrap();
        let txn = g
            .transactions
            .get_mut(txn_id)
            .expect("checkout transaction was created through the API");
        txn["status"] = "completed".into();
        txn["customer_id"] = customer_id.into();
        txn["subscription_id"] = sub_id.into();
        let items: Vec<serde_json::Value> = txn["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|i| {
                serde_json::json!({
                    "status": "active",
                    "quantity": i["quantity"],
                    "price": { "id": i["price_id"] }
                })
            })
            .collect();
        let now = OffsetDateTime::now_utc();
        let sub = serde_json::json!({
            "id": sub_id,
            "status": "active",
            "customer_id": customer_id,
            "items": items,
            "current_billing_period": {
                "starts_at": rfc3339(now),
                "ends_at": rfc3339(now + time::Duration::days(30))
            },
            "scheduled_change": null,
            "canceled_at": null,
            "custom_data": txn["custom_data"].clone(),
            "created_at": rfc3339(now),
            "updated_at": rfc3339(now)
        });
        g.subscriptions.insert(sub_id.to_string(), sub.clone());
        let mut data = sub;
        data["transaction_id"] = txn_id.into();
        event("subscription.created", data)
    }

    /// An `adjustment.created` (refund) webhook for transaction `txn_id`. It
    /// names the transaction but says nothing about its checkout being paid.
    pub fn adjustment_event(&self, txn_id: &str) -> serde_json::Value {
        event(
            "adjustment.created",
            serde_json::json!({
                "id": new_id("adj"),
                "action": "refund",
                "status": "pending_approval",
                "transaction_id": txn_id
            }),
        )
    }

    /// Changes the canonical subscription (as a customer action or Paddle's
    /// own lifecycle would) and returns the `event_type` webhook for it.
    pub fn update_subscription(
        &self,
        sub_id: &str,
        event_type: &str,
        change: impl FnOnce(&mut serde_json::Value),
    ) -> serde_json::Value {
        let mut g = self.inner.lock().unwrap();
        let sub = g.subscriptions.get_mut(sub_id).expect("known subscription");
        change(sub);
        touch(sub);
        event(event_type, sub.clone())
    }

    /// The canonical subscription entity, as `GET /subscriptions/{id}` returns it.
    pub fn subscription(&self, sub_id: &str) -> Option<serde_json::Value> {
        self.inner
            .lock()
            .unwrap()
            .subscriptions
            .get(sub_id)
            .cloned()
    }

    /// The transaction entity as Paddle holds it.
    pub fn transaction(&self, txn_id: &str) -> Option<serde_json::Value> {
        self.inner.lock().unwrap().transactions.get(txn_id).cloned()
    }

    /// Transactions a customer could still pay (not canceled, not paid).
    pub fn payable_transactions(&self) -> Vec<String> {
        let g = self.inner.lock().unwrap();
        g.transactions
            .iter()
            .filter(|(_, t)| matches!(t["status"].as_str(), Some("draft" | "ready")))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// How many times the service called endpoint `name`.
    pub fn calls(&self, name: &str) -> usize {
        *self.inner.lock().unwrap().calls.get(name).unwrap_or(&0)
    }

    /// The next `n` `GET /subscriptions/{id}` calls answer 503.
    pub fn fail_next_gets(&self, n: usize) {
        self.inner.lock().unwrap().failing_gets = n;
    }

    /// The next `POST /transactions` call fails as `failure`.
    pub fn fail_next_create(&self, failure: CreateFailure) {
        self.inner
            .lock()
            .unwrap()
            .create_failures
            .push_back(failure);
    }

    /// The API key loses `permission` (`transaction.read` or
    /// `subscription.read`): every read it covers answers `403 forbidden`.
    pub fn revoke_permission(&self, permission: &'static str) {
        self.revoke(permission, None, true);
    }

    /// Paddle refuses the next `reads` reads covered by `permission` with
    /// `403 forbidden`, then accepts them again (a brief incident).
    pub fn revoke_permission_briefly(&self, permission: &'static str, reads: usize) {
        self.revoke(permission, Some(reads), true);
    }

    /// Something in front of Paddle (an API gateway or WAF) answers every
    /// read covered by `permission` with a `403` JSON error of its own: it
    /// has an `error.code`, but not Paddle's error shape (`meta.request_id`).
    pub fn deny_before_paddle(&self, permission: &'static str) {
        self.revoke(permission, None, false);
    }

    /// The API key gets `permission` back.
    pub fn restore_permission(&self, permission: &'static str) {
        self.inner.lock().unwrap().revoked.remove(permission);
    }

    fn revoke(&self, permission: &'static str, remaining: Option<usize>, by_paddle: bool) {
        self.inner.lock().unwrap().revoked.insert(
            permission,
            Revocation {
                remaining,
                by_paddle,
            },
        );
    }

    /// Changes transaction `txn_id` as Paddle holds it.
    pub fn edit_transaction(&self, txn_id: &str, change: impl FnOnce(&mut serde_json::Value)) {
        let mut g = self.inner.lock().unwrap();
        change(g.transactions.get_mut(txn_id).expect("known transaction"));
    }

    /// Paddle no longer finds transaction `txn_id` (`404`). Returns it as
    /// it was, for [`PaddleFixture::restore_transaction`].
    pub fn forget_transaction(&self, txn_id: &str) -> serde_json::Value {
        self.inner
            .lock()
            .unwrap()
            .transactions
            .remove(txn_id)
            .expect("known transaction")
    }

    /// Paddle finds transaction `txn_id` again, as `entity`.
    pub fn restore_transaction(&self, txn_id: &str, entity: serde_json::Value) {
        self.inner
            .lock()
            .unwrap()
            .transactions
            .insert(txn_id.to_string(), entity);
    }

    /// Paddle cancels transaction `txn_id` on its own side (a dashboard
    /// action, or a cancel whose answer the service never got).
    pub fn cancel_at_paddle(&self, txn_id: &str) {
        let mut g = self.inner.lock().unwrap();
        let txn = g.transactions.get_mut(txn_id).expect("known transaction");
        txn["status"] = "canceled".into();
    }
}

fn touch(sub: &mut serde_json::Value) {
    // Paddle timestamps have sub-second precision; keep updates ordered
    // even when two happen within the same second.
    sub["updated_at"] = OffsetDateTime::now_utc().format(&Rfc3339).unwrap().into();
}

fn event(event_type: &str, data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "event_id": new_id("evt"),
        "event_type": event_type,
        "occurred_at": OffsetDateTime::now_utc().format(&Rfc3339).unwrap(),
        "notification_id": new_id("ntf"),
        "data": data
    })
}

pub fn sign_payload(secret: &str, body: &[u8], ts: i64) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac key");
    let ts_str = ts.to_string();
    mac.update(ts_str.as_bytes());
    mac.update(b":");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let hex_sig: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("ts={ts};h1={hex_sig}")
}

pub async fn start() -> PaddleFixture {
    let inner: Shared = Arc::new(Mutex::new(Inner::default()));

    let app = Router::new()
        .route(
            "/transactions",
            get(list_transactions).post(create_transaction),
        )
        .route("/subscriptions", get(list_subscriptions))
        .route(
            "/transactions/{transaction_id}",
            get(get_transaction).patch(update_transaction),
        )
        .route(
            "/customers/{customer_id}/portal-sessions",
            post(create_portal_session),
        )
        .route(
            "/subscriptions/{subscription_id}/cancel",
            post(cancel_subscription),
        )
        .route(
            "/subscriptions/{subscription_id}",
            get(get_subscription).patch(update_subscription),
        )
        .with_state(inner.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .ok();
    });

    PaddleFixture {
        base_url,
        inner,
        _task: Arc::new(task),
    }
}

fn authorized(headers: &HeaderMap) -> bool {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    auth == Some(&format!("Bearer {API_KEY}"))
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": {
                "type": "request_error",
                "code": "authentication_malformed",
                "detail": "Authentication header included, but incorrectly formatted"
            },
            "meta": { "request_id": new_id("req") }
        })),
    )
        .into_response()
}

/// `403` when the key lacks `permission`, as Paddle answers it.
fn forbidden(inner: &Shared, permission: &str) -> Option<Response> {
    let mut g = inner.lock().unwrap();
    let revocation = g.revoked.get_mut(permission)?;
    match &mut revocation.remaining {
        Some(0) => return None,
        Some(n) => *n -= 1,
        None => {}
    }
    if !revocation.by_paddle {
        return Some(
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": {
                        "code": "AuthorizationFailed",
                        "message": "The client is not authorized by the gateway policy."
                    }
                })),
            )
                .into_response(),
        );
    }
    Some(
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": {
                    "type": "request_error",
                    "code": "forbidden",
                    "detail": format!("You aren't permitted to perform this request: {permission}")
                },
                "meta": { "request_id": new_id("req") }
            })),
        )
            .into_response(),
    )
}

/// A Paddle list page of `entities` (only the first `per_page` are kept).
fn list_page(entities: Vec<serde_json::Value>) -> Response {
    let entities: Vec<serde_json::Value> = entities.into_iter().take(1).collect();
    Json(serde_json::json!({
        "data": entities,
        "meta": { "pagination": { "per_page": 1, "has_more": false } }
    }))
    .into_response()
}

async fn list_transactions(State(inner): State<Shared>, headers: HeaderMap) -> Response {
    count(&inner, "list_transactions");
    if !authorized(&headers) {
        return unauthorized();
    }
    if let Some(denied) = forbidden(&inner, "transaction.read") {
        return denied;
    }
    let all = inner
        .lock()
        .unwrap()
        .transactions
        .values()
        .cloned()
        .collect();
    list_page(all)
}

async fn list_subscriptions(State(inner): State<Shared>, headers: HeaderMap) -> Response {
    count(&inner, "list_subscriptions");
    if !authorized(&headers) {
        return unauthorized();
    }
    if let Some(denied) = forbidden(&inner, "subscription.read") {
        return denied;
    }
    let all = inner
        .lock()
        .unwrap()
        .subscriptions
        .values()
        .cloned()
        .collect();
    list_page(all)
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "entity not found").into_response()
}

fn count(inner: &Shared, name: &'static str) {
    *inner.lock().unwrap().calls.entry(name).or_default() += 1;
}

async fn create_transaction(
    State(inner): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    count(&inner, "create_transaction");
    if !authorized(&headers) {
        return unauthorized();
    }
    let failure = inner.lock().unwrap().create_failures.pop_front();
    if let Some(CreateFailure::Unavailable) = failure {
        return (StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable").into_response();
    }
    let txn_id = new_id("txn");
    let checkout_url = format!("https://sandbox-checkout.paddle.com/checkout/{txn_id}");
    let mut data = serde_json::json!({
        "id": txn_id,
        "status": "draft",
        "customer_id": body.get("customer_id"),
        "custom_data": body.get("custom_data"),
        "items": body.get("items"),
        "checkout": {
            "url": checkout_url
        }
    });
    if let Some(CreateFailure::WithoutCheckoutUrl) = failure {
        data["checkout"] = serde_json::Value::Null;
    }
    inner
        .lock()
        .unwrap()
        .transactions
        .insert(txn_id, data.clone());
    if let Some(CreateFailure::Hang(delay)) = failure {
        tokio::time::sleep(delay).await;
    }
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "data": data })),
    )
        .into_response()
}

async fn create_portal_session(
    State(inner): State<Shared>,
    Path(customer_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    count(&inner, "portal_session");
    if !authorized(&headers) {
        return unauthorized();
    }
    let p_id = new_id("cpls");
    let portal_url =
        format!("https://customer-portal.paddle.com/{p_id}?action=overview&token=test");
    let response = serde_json::json!({
        "data": {
            "id": p_id,
            "customer_id": customer_id,
            "urls": {
                "general": {
                    "overview": portal_url
                }
            }
        }
    });
    (StatusCode::CREATED, Json(response)).into_response()
}

async fn get_subscription(
    State(inner): State<Shared>,
    Path(sub_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    count(&inner, "get_subscription");
    if !authorized(&headers) {
        return unauthorized();
    }
    if let Some(denied) = forbidden(&inner, "subscription.read") {
        return denied;
    }
    let mut g = inner.lock().unwrap();
    if g.failing_gets > 0 {
        g.failing_gets -= 1;
        return (StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable").into_response();
    }
    match g.subscriptions.get(&sub_id) {
        Some(sub) => Json(serde_json::json!({ "data": sub })).into_response(),
        None => not_found(),
    }
}

async fn cancel_subscription(
    State(inner): State<Shared>,
    Path(sub_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    count(&inner, "cancel");
    if !authorized(&headers) {
        return unauthorized();
    }
    let mut g = inner.lock().unwrap();
    let Some(sub) = g.subscriptions.get_mut(&sub_id) else {
        return not_found();
    };
    let effective_from = body
        .get("effective_from")
        .and_then(|v| v.as_str())
        .unwrap_or("next_billing_period");
    if effective_from == "immediately" {
        sub["status"] = "canceled".into();
        sub["canceled_at"] = now_rfc3339().into();
        sub["scheduled_change"] = serde_json::Value::Null;
    } else {
        let ends_at = sub["current_billing_period"]["ends_at"].clone();
        sub["scheduled_change"] = serde_json::json!({
            "action": "cancel",
            "effective_at": ends_at,
            "resume_at": null
        });
    }
    touch(sub);
    Json(serde_json::json!({ "data": sub.clone() })).into_response()
}

async fn update_subscription(
    State(inner): State<Shared>,
    Path(sub_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    count(&inner, "update");
    if !authorized(&headers) {
        return unauthorized();
    }
    if body.get("proration_billing_mode").is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "proration_billing_mode is required when items change",
        )
            .into_response();
    }
    let mut g = inner.lock().unwrap();
    let Some(sub) = g.subscriptions.get_mut(&sub_id) else {
        return not_found();
    };
    if let Some(items) = body.get("items").and_then(|v| v.as_array()) {
        sub["items"] = items
            .iter()
            .map(|i| {
                serde_json::json!({
                    "status": "active",
                    "quantity": i["quantity"],
                    "price": { "id": i["price_id"] }
                })
            })
            .collect();
    }
    touch(sub);
    Json(serde_json::json!({ "data": sub.clone() })).into_response()
}

/// `PATCH /transactions/{id}`: only cancellation is modelled. Paddle:
/// "You can cancel transactions that are draft, ready, billed, or past_due";
/// "billed and completed transactions ... can't be changed";
/// "When canceling, only include status in your request." A transaction
/// that is already canceled is not in the cancelable list either, so a
/// second cancel is refused like any other immutable transaction.
async fn update_transaction(
    State(inner): State<Shared>,
    Path(txn_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    count(&inner, "cancel_transaction");
    if !authorized(&headers) {
        return unauthorized();
    }
    let only_status = body.as_object().is_some_and(|o| o.len() == 1)
        && body["status"].as_str() == Some("canceled");
    if !only_status {
        return (StatusCode::BAD_REQUEST, "only status=canceled is supported").into_response();
    }
    let mut g = inner.lock().unwrap();
    let Some(txn) = g.transactions.get_mut(&txn_id) else {
        return not_found();
    };
    match txn["status"].as_str().unwrap_or_default() {
        "draft" | "ready" | "billed" | "past_due" => {
            txn["status"] = "canceled".into();
            Json(serde_json::json!({ "data": txn.clone() })).into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": { "type": "request_error", "code": "transaction_immutable" }
            })),
        )
            .into_response(),
    }
}

async fn get_transaction(
    State(inner): State<Shared>,
    Path(txn_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    count(&inner, "get_transaction");
    if !authorized(&headers) {
        return unauthorized();
    }
    if let Some(denied) = forbidden(&inner, "transaction.read") {
        return denied;
    }
    match inner.lock().unwrap().transactions.get(&txn_id) {
        Some(txn) => Json(serde_json::json!({ "data": txn })).into_response(),
        None => not_found(),
    }
}
