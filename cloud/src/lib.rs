//! Muster Cloud service: multi-tenant hosted transport for Muster fleets.
#![forbid(unsafe_code)]

/// Wire protocol shared with the CLI. Compiled from the root crate's source
/// file so both sides use the same definitions without the service having to
/// wait for `muster::cloud_protocol` to be registered in the root `lib.rs`.
#[path = "../../src/cloud_protocol.rs"]
pub mod protocol;

pub mod auth;
pub mod billing;
pub mod billing_reconcile;
pub mod config;
pub mod control;
pub mod device_links;
pub mod error;
pub mod history;
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
            get(tenancy::detail).delete(tenancy::delete_workspace),
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
        .layer(middleware::from_fn(error::request_id_layer))
        .with_state(state)
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
    Ok(Json(HealthResponse {
        status: "ready".into(),
    }))
}
