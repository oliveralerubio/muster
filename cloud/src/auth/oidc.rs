//! OpenID Connect Authorization Code + PKCE (S256) against the configured
//! provider. The service holds no test hooks: tests run a separate IdP.

use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    reqwest, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet, EndpointSet,
    IssuerUrl, Nonce, PkceCodeChallenge, RedirectUrl, Scope,
};
use serde::Deserialize;

use super::{cookie_header, random_token, sha256};
use crate::config::Config;
use crate::error::AppError;
use crate::protocol::{routes, COOKIE_LOGIN};
use crate::state::AppState;

/// Client built from discovery: auth URL set, token/userinfo from metadata.
pub type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// How long a started login may take before the callback is refused.
const LOGIN_TTL_SECS: i64 = 600;

pub struct Oidc {
    pub client: Client,
    /// HTTP client for back-channel calls; never follows redirects (SSRF).
    pub http: reqwest::Client,
}

impl Oidc {
    pub async fn discover(config: &Config) -> Result<Self, String> {
        let http = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|err| format!("http client: {err}"))?;
        let issuer = IssuerUrl::new(
            config
                .oidc_issuer
                .as_str()
                .trim_end_matches('/')
                .to_string(),
        )
        .map_err(|err| format!("issuer: {err}"))?;
        let metadata = CoreProviderMetadata::discover_async(issuer, &http)
            .await
            .map_err(|err| format!("OIDC discovery failed: {err}"))?;
        let redirect = config
            .public_url
            .join(routes::AUTH_CALLBACK)
            .map_err(|err| format!("redirect URL: {err}"))?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(config.oidc_client_id.clone()),
            Some(ClientSecret::new(config.oidc_client_secret().to_string())),
        )
        .set_redirect_uri(RedirectUrl::from_url(redirect));
        Ok(Self { client, http })
    }
}

#[derive(Deserialize)]
pub struct LoginQuery {
    return_to: Option<String>,
}

/// Only same-origin absolute paths; anything else (`//host`, `/\host`,
/// schemes) falls back to `/` so login cannot become an open redirect.
pub fn safe_return_to(raw: Option<&str>) -> String {
    match raw {
        Some(path)
            if path.starts_with('/')
                && !path.starts_with("//")
                && !path.contains('\\')
                && !path.chars().any(char::is_control) =>
        {
            path.to_string()
        }
        _ => "/".to_string(),
    }
}

/// `GET /auth/login`: records the attempt and redirects to the provider.
pub async fn login(
    State(state): State<AppState>,
    Query(query): Query<LoginQuery>,
) -> Result<Response, AppError> {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, csrf, nonce) = state
        .oidc
        .client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".into()))
        .set_pkce_challenge(challenge)
        .url();

    let browser = random_token();
    sqlx::query(
        "INSERT INTO oidc_logins (state_hash, browser_hash, pkce_verifier, nonce, return_to, expires_at)
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6))",
    )
    .bind(sha256(csrf.secret().as_bytes()))
    .bind(sha256(browser.as_bytes()))
    .bind(verifier.secret())
    .bind(nonce.secret())
    .bind(safe_return_to(query.return_to.as_deref()))
    .bind(LOGIN_TTL_SECS as f64)
    .execute(&state.db)
    .await?;

    let location = HeaderValue::from_str(url.as_str()).map_err(AppError::internal)?;
    let cookie = cookie_header(COOKIE_LOGIN, &browser, LOGIN_TTL_SECS);
    Ok((
        StatusCode::SEE_OTHER,
        [(header::LOCATION, location), (header::SET_COOKIE, cookie)],
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// How long a browser session lasts.
pub const SESSION_TTL_SECS: i64 = 7 * 24 * 3600;

/// `GET /auth/callback`: consumes the login attempt (single use), exchanges
/// the code with the PKCE verifier, verifies the ID token (signature, iss,
/// aud, exp, nonce), upserts the account by (issuer, subject) and opens a
/// session.
pub async fn callback(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, AppError> {
    if let Some(error) = query.error {
        return Err(AppError::bad_request("identity provider refused the login")
            .with_details(serde_json::json!({ "provider_error": error })));
    }
    let (Some(code), Some(csrf)) = (query.code, query.state) else {
        return Err(AppError::bad_request("callback needs code and state"));
    };
    let browser = super::read_cookie(&headers, COOKIE_LOGIN)
        .ok_or_else(|| AppError::csrf_failed("login was not started in this browser"))?;

    let row: Option<(Vec<u8>, String, String, String)> = sqlx::query_as(
        "DELETE FROM oidc_logins WHERE state_hash = $1 AND expires_at > now()
         RETURNING browser_hash, pkce_verifier, nonce, return_to",
    )
    .bind(sha256(csrf.as_bytes()))
    .fetch_optional(&state.db)
    .await?;
    let Some((browser_hash, verifier, nonce, return_to)) = row else {
        return Err(AppError::csrf_failed("login attempt unknown or expired"));
    };
    if !super::constant_time_eq(&browser_hash, &sha256(browser.as_bytes())) {
        return Err(AppError::csrf_failed(
            "login was not started in this browser",
        ));
    }

    let token = state
        .oidc
        .client
        .exchange_code(openidconnect::AuthorizationCode::new(code))
        .map_err(AppError::internal)?
        .set_pkce_verifier(openidconnect::PkceCodeVerifier::new(verifier))
        .request_async(&state.oidc.http)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "OIDC code exchange failed");
            AppError::bad_request("identity provider rejected the login")
        })?;
    use openidconnect::TokenResponse;
    let id_token = token
        .id_token()
        .ok_or_else(|| AppError::bad_request("identity provider returned no ID token"))?;
    let claims = id_token
        .claims(&state.oidc.client.id_token_verifier(), &Nonce::new(nonce))
        .map_err(|err| {
            tracing::warn!(error = %err, "ID token rejected");
            AppError::bad_request("identity provider returned an invalid ID token")
        })?;
    let email = claims
        .email()
        .map(|e| e.as_str().to_string())
        .ok_or_else(|| AppError::bad_request("identity provider did not share an email address"))?;
    // The email governs invitation acceptance, so only an address the
    // provider vouches for (`email_verified: true`) is accepted. Missing or
    // false is refused; there is no development bypass.
    if claims.email_verified() != Some(true) {
        return Err(AppError::bad_request(
            "identity provider has not verified this email address",
        ));
    }

    let (user_id,): (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO users (id, issuer, subject, email) VALUES ($1, $2, $3, $4)
         ON CONFLICT (issuer, subject) DO UPDATE SET email = EXCLUDED.email
         RETURNING id",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(claims.issuer().as_str())
    .bind(claims.subject().as_str())
    .bind(&email)
    .fetch_one(&state.db)
    .await?;

    let session = random_token();
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, csrf_token, expires_at)
         VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(sha256(session.as_bytes()))
    .bind(user_id)
    .bind(random_token())
    .bind(SESSION_TTL_SECS as f64)
    .execute(&state.db)
    .await?;

    let location =
        HeaderValue::from_str(&safe_return_to(Some(&return_to))).map_err(AppError::internal)?;
    let mut response = (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response();
    let headers = response.headers_mut();
    headers.append(
        header::SET_COOKIE,
        cookie_header(crate::protocol::COOKIE_SESSION, &session, SESSION_TTL_SECS),
    );
    headers.append(header::SET_COOKIE, cookie_header(COOKIE_LOGIN, "", 0));
    Ok(response)
}
