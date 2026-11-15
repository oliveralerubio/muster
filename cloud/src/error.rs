//! API errors. Handlers return [`AppError`]; the request-id middleware turns
//! it into the protocol envelope `{"error":{code,message,details},"request_id"}`
//! so the id in the body always matches the `X-Request-Id` header.

use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::protocol::{error_codes, ApiError, ApiErrorBody, HEADER_REQUEST_ID};

#[derive(Debug, Clone)]
pub struct AppError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub details: serde_json::Value,
}

impl AppError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: serde_json::json!({}),
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, error_codes::BAD_REQUEST, message)
    }

    pub fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            error_codes::UNAUTHENTICATED,
            "sign in required",
        )
    }

    pub fn csrf_failed(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, error_codes::CSRF_FAILED, message)
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, "not found")
    }

    pub fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            error_codes::METHOD_NOT_ALLOWED,
            "method not allowed",
        )
    }

    pub fn idempotency_mismatch() -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            error_codes::IDEMPOTENCY_MISMATCH,
            "Idempotency-Key was already used for a different request",
        )
    }

    pub fn not_ready(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::NOT_READY,
            message,
        )
    }

    pub fn billing_unconfigured() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BILLING_UNCONFIGURED,
            "billing provider is not configured on this server",
        )
    }

    /// Logs the cause server-side; the client only sees a generic message.
    pub fn internal(cause: impl std::fmt::Display) -> Self {
        tracing::error!(error = %cause, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            error_codes::INTERNAL,
            "internal error",
        )
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        AppError::internal(format_args!("database: {err}"))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let mut response = self.status.into_response();
        response.extensions_mut().insert(self);
        response
    }
}

/// Assigns a fresh request id, exposes it as `X-Request-Id`, and renders any
/// [`AppError`] with that id.
pub async fn request_id_layer(mut request: Request, next: Next) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    request
        .extensions_mut()
        .insert(RequestId(request_id.clone()));
    let mut response = next.run(request).await;
    if let Some(err) = response.extensions_mut().remove::<AppError>() {
        let body = ApiError {
            error: ApiErrorBody {
                code: err.code.to_string(),
                message: err.message,
                details: err.details,
            },
            request_id: request_id.clone(),
        };
        let headers = std::mem::take(response.headers_mut());
        response = (err.status, Json(body)).into_response();
        for (name, value) in headers.iter() {
            if name != axum::http::header::CONTENT_TYPE
                && name != axum::http::header::CONTENT_LENGTH
            {
                response.headers_mut().append(name.clone(), value.clone());
            }
        }
    }
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(HEADER_REQUEST_ID, value);
    }
    response
}

#[derive(Debug, Clone)]
pub struct RequestId(pub String);
