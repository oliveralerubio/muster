//! Muster Cloud service: multi-tenant hosted transport for Muster fleets.
// `deny`, not `forbid`: only `dev_crash` opts in, for one `kill(2)` call.
#![deny(unsafe_code)]

/// Wire protocol shared with the CLI. Compiled from the root crate's source
/// file so both sides use the same definitions without the service having to
/// wait for `muster::cloud_protocol` to be registered in the root `lib.rs`.
#[path = "../../src/cloud_protocol.rs"]
pub mod protocol;

pub mod auth;
pub mod backup;
pub mod billing;
pub mod billing_reconcile;
pub mod config;
pub mod control;
pub mod dev_crash;
pub mod device_links;
pub mod error;
pub mod history;
pub mod lifecycle;
pub mod lifecycle_provider;
pub mod lifecycle_purge;
pub mod members;
pub mod proposals;
pub mod receipts;
pub mod repo_store;
pub mod state;
pub mod sync_api;
pub mod tenancy;
pub mod web;

use axum::extract::State;
use axum::{middleware, routing::get, Json, Router};

use error::AppError;
use protocol::HealthResponse;
use state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route(protocol::routes::HEALTHZ, get(healthz))
        .route(protocol::routes::READYZ, get(readyz))
        .route(protocol::routes::AUTH_LOGIN, get(auth::oidc::login))
        .route(protocol::routes::AUTH_CALLBACK, get(auth::oidc::callback))
        .route(protocol::routes::ME, get(auth::session::me))
        .route(
            protocol::routes::LOGOUT,
            axum::routing::post(auth::session::logout),
        )
        .route(
            protocol::routes::WORKSPACES,
            get(tenancy::list).post(tenancy::create),
        )
        .route(
            protocol::routes::WORKSPACE,
            get(tenancy::detail).delete(lifecycle::request_deletion),
        )
        .route(
            protocol::routes::WORKSPACE_LIFECYCLE,
            get(lifecycle::get_lifecycle),
        )
        .route(
            protocol::routes::WORKSPACE_MEMBERS,
            get(members::list_members),
        )
        .route(
            protocol::routes::WORKSPACE_MEMBER,
            axum::routing::delete(members::remove_member),
        )
        .route(
            protocol::routes::WORKSPACE_INVITATIONS,
            get(members::list_invitations).post(members::create_invitation),
        )
        .route(
            protocol::routes::INVITATION_ACCEPT,
            axum::routing::post(members::accept_invitation),
        )
        .route(
            protocol::routes::DEVICE_AUTHORIZE,
            axum::routing::post(auth::device_flow::authorize),
        )
        .route(
            protocol::routes::DEVICE_APPROVE,
            axum::routing::post(auth::device_flow::approve),
        )
        .route(
            protocol::routes::DEVICE_TOKEN,
            axum::routing::post(auth::device_flow::token),
        )
        .route(
            protocol::routes::WORKSPACE_DEVICES,
            get(device_links::list_devices).post(device_links::link_device),
        )
        .route(
            protocol::routes::WORKSPACE_PROPOSALS,
            get(proposals::list).post(proposals::create),
        )
        .route(protocol::routes::WORKSPACE_PROPOSAL, get(proposals::detail))
        .route(
            protocol::routes::WORKSPACE_PROPOSAL_RESOLVE,
            axum::routing::post(proposals::resolve),
        )
        .route(
            protocol::routes::WORKSPACE_PROPOSAL_BUNDLE,
            get(proposals::bundle),
        )
        .route(
            protocol::routes::WORKSPACE_RECEIPTS,
            axum::routing::post(receipts::submit_receipt),
        )
        .route(
            protocol::routes::WORKSPACE_REVISIONS,
            get(history::list_revisions),
        )
        .route(
            protocol::routes::WORKSPACE_REVISION_DIFF,
            get(history::diff_revision),
        )
        .route(
            protocol::routes::WORKSPACE_EXPORT,
            get(history::export_bundle),
        )
        .route(
            protocol::routes::WORKSPACE_BILLING,
            get(billing::get_billing),
        )
        .route(
            protocol::routes::WORKSPACE_BILLING_CHECKOUT,
            axum::routing::post(billing::create_checkout),
        )
        .route(
            protocol::routes::WORKSPACE_BILLING_PORTAL,
            axum::routing::post(billing::customer_portal),
        )
        .route(
            protocol::routes::WORKSPACE_BILLING_CANCEL,
            axum::routing::post(billing::cancel_subscription),
        )
        .route(
            protocol::routes::WORKSPACE_BILLING_PLAN,
            axum::routing::post(billing::update_plan),
        )
        .route(
            protocol::routes::BILLING_WEBHOOK,
            axum::routing::post(billing::handle_webhook),
        )
        .route(
            protocol::routes::WORKSPACE_SYNC,
            get(sync_api::get_sync).post(sync_api::post_sync).layer(
                axum::extract::DefaultBodyLimit::max(sync_api::MAX_BUNDLE_BYTES),
            ),
        )
        .merge(web::routes())
        .fallback(|| async { AppError::not_found() })
        .method_not_allowed_fallback(|| async { AppError::method_not_allowed() })
        .layer(middleware::from_fn_with_state(state.clone(), fence_layer))
        .layer(middleware::from_fn(error::request_id_layer))
        .with_state(state)
}

/// Write (and read) fencing: a running service whose database incarnation
/// is no longer the active one — a restore was activated in its place —
/// refuses every request but liveness and readiness (which reports it),
/// so it can neither accept writes nor serve stale or deleted data.
async fn fence_layer(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = req.uri().path();
    if path == protocol::routes::HEALTHZ || path == protocol::routes::READYZ {
        return next.run(req).await;
    }
    let write = req.method() != axum::http::Method::GET && req.method() != axum::http::Method::HEAD;
    if let Err(err) = state
        .role_gate
        .check(&state.config.control, &state.db)
        .await
    {
        tracing::warn!(error = %err, "fenced: request refused");
        return AppError::not_ready(err.to_string()).into_response();
    }
    let response = next.run(req).await;
    if write {
        if let Some(pause) = &state.config.dev_ack_pause_file {
            dev_ack_pause(pause).await;
        }
    }
    // Checked again right before acknowledging: a request that entered
    // before another incarnation was activated (a long push, say) is never
    // acknowledged by the fenced instance. Its client gets 503 and retries
    // against the active one; what it changed here is on a database that
    // will never serve again.
    if let Err(err) = state
        .role_gate
        .check(&state.config.control, &state.db)
        .await
    {
        tracing::warn!(error = %err, "fenced while handling: acknowledgement withheld");
        return AppError::not_ready(err.to_string()).into_response();
    }
    response
}

/// Development hook (`MUSTER_DEV_ACK_PAUSE_FILE`, refused in production).
async fn dev_ack_pause(pause: &std::path::Path) {
    if !pause.exists() {
        return;
    }
    let _ = std::fs::write(pause.with_extension("reached"), b"");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while pause.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
    })
}

async fn readyz(State(state): State<AppState>) -> Result<Json<HealthResponse>, AppError> {
    sqlx::query("SELECT 1")
        .execute(&state.db)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "readiness: database unreachable");
            AppError::not_ready("database unreachable")
        })?;
    control::check_for_service(&state.config.control, &state.db)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "readiness: lifecycle authority");
            AppError::not_ready(err.to_string())
        })?;
    // A purged workspace whose rows or files came back (restore) is not
    // ready until the purge worker erases them again.
    let resurrected = lifecycle_purge::resurrected_present(&state.db, &state.config.repo_root)
        .await
        .map_err(|e| AppError::not_ready(format!("purge check: {e}")))?;
    if resurrected {
        return Err(AppError::not_ready(
            "a purged workspace has restored data still being erased",
        ));
    }
    Ok(Json(HealthResponse {
        status: "ready".into(),
    }))
}
