//! Web console routes and embedded static assets for Muster Cloud.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::state::AppState;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");

const CSP_HEADER: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";

/// Composable router for the Muster Cloud web console.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/workspaces", get(index))
        .route("/workspaces/{ws}", get(index))
        .route("/device", get(index))
        .route("/assets/app.css", get(css))
        .route("/assets/app.js", get(js))
}

async fn index() -> Response {
    asset_response(
        "text/html; charset=utf-8",
        INDEX_HTML,
        "no-store",
        Some(CSP_HEADER),
    )
}

async fn css() -> Response {
    asset_response("text/css; charset=utf-8", APP_CSS, "no-cache", None)
}

async fn js() -> Response {
    asset_response(
        "application/javascript; charset=utf-8",
        APP_JS,
        "no-cache",
        None,
    )
}

fn asset_response(
    content_type: &'static str,
    body: &'static str,
    cache_control: &'static str,
    csp: Option<&'static str>,
) -> Response {
    let mut resp = (StatusCode::OK, body).into_response();
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Some(policy) = csp {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(policy),
        );
    }
    resp
}
