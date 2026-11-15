//! Synthetic external Paddle Billing provider fixture for integration tests.
//! Runs a real HTTP server on a loopback port implementing the endpoints
//! the service calls (`POST /transactions`, `POST /portal-sessions`,
//! `POST /subscriptions/{id}/cancel`, `PATCH /subscriptions/{id}`).
//!
//! Generates real `Paddle-Signature` headers with HMAC-SHA256 for testing
//! the webhook endpoint.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const API_KEY: &str =
    "pdl_sdbx_apikey_01gtgztp8f4kek3yd4g1wrksa3_test1234567890123456789012_abc";
pub const WEBHOOK_SECRET: &str =
    "pdl_ntfset_01gkpjp8bkm3tm53kdgkx6sms7_secret123456789012345678901234";
pub const PRICE_PERSONAL: &str = "pri_01h8vzp8f4kek3yd4g1wrksa3p";
pub const PRICE_TEAM_SEAT: &str = "pri_01h8vzp8f4kek3yd4g1wrksa3t";

type HmacSha256 = Hmac<Sha256>;

#[derive(Default)]
struct Inner {
    transactions: HashMap<String, serde_json::Value>,
    portal_sessions: HashMap<String, serde_json::Value>,
    subscriptions: HashMap<String, serde_json::Value>,
}

#[derive(Clone)]
pub struct PaddleFixture {
    pub base_url: String,
    _task: Arc<tokio::task::JoinHandle<()>>,
}

impl PaddleFixture {
    /// Generates a valid `Paddle-Signature` header for `body` at timestamp `ts`.
    pub fn sign_webhook(&self, body: &[u8], ts: i64) -> String {
        sign_payload(WEBHOOK_SECRET, body, ts)
    }

    /// Generates a valid webhook payload for `subscription.created` or `subscription.updated`.
    pub fn subscription_event(
        &self,
        event_id: &str,
        event_type: &str,
        sub_id: &str,
        customer_id: &str,
        ws_id: &str,
        plan: &str,
        seats: u32,
        status: &str,
        occurred_at: &str,
        period_start: &str,
        period_end: &str,
    ) -> serde_json::Value {
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
                "scheduled_change": null
            }
        })
    }
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
    let inner = Arc::new(Mutex::new(Inner::default()));

    let app = Router::new()
        .route("/transactions", post(create_transaction))
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
            axum::routing::patch(update_subscription),
        )
        .with_state(inner);

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
        _task: Arc::new(task),
    }
}

fn check_auth(headers: &HeaderMap) -> Result<(), Response> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if auth == Some(&format!("Bearer {API_KEY}")) {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "invalid api key").into_response())
    }
}

async fn create_transaction(
    State(inner): State<Arc<Mutex<Inner>>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(r) = check_auth(&headers) {
        return r;
    }
    let txn_id = format!("txn_{}", uuid::Uuid::new_v4().simple());
    let checkout_url = format!("https://sandbox-checkout.paddle.com/checkout/{txn_id}");
    let response = serde_json::json!({
        "data": {
            "id": txn_id,
            "status": "draft",
            "custom_data": body.get("custom_data"),
            "items": body.get("items"),
            "checkout": {
                "url": checkout_url
            }
        }
    });
    inner
        .lock()
        .unwrap()
        .transactions
        .insert(txn_id, response.clone());
    (StatusCode::CREATED, Json(response)).into_response()
}

async fn create_portal_session(
    State(inner): State<Arc<Mutex<Inner>>>,
    Path(customer_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(r) = check_auth(&headers) {
        return r;
    }
    let p_id = format!("cpl_{}", uuid::Uuid::new_v4().simple());
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
    inner.lock().unwrap().portal_sessions.insert(p_id, body);
    (StatusCode::CREATED, Json(response)).into_response()
}

async fn cancel_subscription(
    State(inner): State<Arc<Mutex<Inner>>>,
    Path(sub_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = check_auth(&headers) {
        return r;
    }
    let response = serde_json::json!({
        "data": {
            "id": sub_id,
            "status": "active",
            "scheduled_change": {
                "action": "cancel",
                "effective_at": "2026-12-14T00:00:00Z"
            }
        }
    });
    inner
        .lock()
        .unwrap()
        .subscriptions
        .insert(sub_id, response.clone());
    (StatusCode::OK, Json(response)).into_response()
}

async fn update_subscription(
    State(inner): State<Arc<Mutex<Inner>>>,
    Path(sub_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(r) = check_auth(&headers) {
        return r;
    }
    let response = serde_json::json!({
        "data": {
            "id": sub_id,
            "status": "active",
            "items": body.get("items")
        }
    });
    inner
        .lock()
        .unwrap()
        .subscriptions
        .insert(sub_id, response.clone());
    (StatusCode::OK, Json(response)).into_response()
}
