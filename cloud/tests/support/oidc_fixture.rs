//! External OpenID Connect provider for tests.
//!
//! A real HTTP server on an ephemeral loopback port, separate from the
//! service under test: discovery, JWKS (RS256 key generated per test
//! process), `authorize` and `token`. The service talks to it exactly as it
//! would to a production IdP; there is no bypass inside the service.
//!
//! `authorize` needs an extra `login_as=<subject>` (and optional `email`)
//! query parameter, which the test appends to the URL the service redirected
//! to: that stands in for the user signing in at the IdP.
//!
//! `token` enforces what a real provider enforces: single-use codes bound to
//! client, redirect URI and PKCE S256 challenge, plus client authentication.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use openidconnect::core::{
    CoreIdToken, CoreIdTokenClaims, CoreJsonWebKeySet, CoreJwsSigningAlgorithm,
    CoreRsaPrivateSigningKey,
};
use openidconnect::{
    Audience, EmptyAdditionalClaims, EndUserEmail, IssuerUrl, JsonWebKeyId, Nonce,
    PrivateSigningKey, StandardClaims, SubjectIdentifier,
};
use sha2::{Digest, Sha256};

pub const CLIENT_ID: &str = "muster-cloud-test";
/// Synthetic test-only value; never a real credential.
pub const CLIENT_SECRET: &str = "test-client-secret-not-real";

struct PendingCode {
    redirect_uri: String,
    code_challenge: String,
    nonce: String,
    subject: String,
    email: Option<String>,
    email_verified: Option<bool>,
}

struct Inner {
    issuer: String,
    codes: Mutex<HashMap<String, PendingCode>>,
}

pub struct Idp {
    pub issuer: String,
    _task: tokio::task::JoinHandle<()>,
}

impl Drop for Idp {
    fn drop(&mut self) {
        self._task.abort();
    }
}

fn signing_key_pem() -> &'static str {
    static PEM: OnceLock<String> = OnceLock::new();
    PEM.get_or_init(|| {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).expect("rsa key");
        key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
            .expect("pem")
            .to_string()
    })
}

fn signing_key() -> CoreRsaPrivateSigningKey {
    CoreRsaPrivateSigningKey::from_pem(
        signing_key_pem(),
        Some(JsonWebKeyId::new("test-key-1".into())),
    )
    .expect("signing key")
}

pub async fn start() -> Idp {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let inner = Arc::new(Inner {
        issuer: issuer.clone(),
        codes: Mutex::new(HashMap::new()),
    });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .with_state(inner);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Idp {
        issuer,
        _task: task,
    }
}

async fn discovery(State(inner): State<Arc<Inner>>) -> Json<serde_json::Value> {
    let iss = &inner.issuer;
    Json(serde_json::json!({
        "issuer": iss,
        "authorization_endpoint": format!("{iss}/authorize"),
        "token_endpoint": format!("{iss}/token"),
        "jwks_uri": format!("{iss}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
        "scopes_supported": ["openid", "email", "profile"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic"],
        "code_challenge_methods_supported": ["S256"],
    }))
}

async fn jwks() -> Json<CoreJsonWebKeySet> {
    Json(CoreJsonWebKeySet::new(vec![
        signing_key().as_verification_key()
    ]))
}

fn bad(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": "invalid_request", "error_description": msg})),
    )
        .into_response()
}

async fn authorize(
    State(inner): State<Arc<Inner>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let get = |k: &str| q.get(k).cloned();
    if get("response_type").as_deref() != Some("code") {
        return bad("response_type must be code");
    }
    if get("client_id").as_deref() != Some(CLIENT_ID) {
        return bad("unknown client_id");
    }
    if get("code_challenge_method").as_deref() != Some("S256") {
        return bad("PKCE S256 required");
    }
    let scopes = get("scope").unwrap_or_default();
    if !scopes.split(' ').any(|s| s == "openid") {
        return bad("openid scope required");
    }
    let (Some(redirect_uri), Some(state), Some(nonce), Some(challenge)) = (
        get("redirect_uri"),
        get("state"),
        get("nonce"),
        get("code_challenge"),
    ) else {
        return bad("redirect_uri, state, nonce and code_challenge are required");
    };
    let Some(subject) = get("login_as") else {
        return bad("fixture needs login_as=<subject>");
    };
    let code = uuid::Uuid::new_v4().simple().to_string();
    inner.codes.lock().unwrap().insert(
        code.clone(),
        PendingCode {
            redirect_uri: redirect_uri.clone(),
            code_challenge: challenge,
            nonce,
            subject,
            email: get("email"),
            // `email_verified=false` or `=missing` simulate a provider that
            // does not vouch for the address; default `true`.
            email_verified: match get("email_verified").as_deref() {
                Some("false") => Some(false),
                Some("missing") => None,
                _ => Some(true),
            },
        },
    );
    let mut target = url::Url::parse(&redirect_uri).expect("redirect_uri");
    target
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &state);
    Redirect::to(target.as_str()).into_response()
}

async fn token(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let expected_basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
    );
    let authorized = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == expected_basic);
    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid_client"})),
        )
            .into_response();
    }
    let grant = || {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid_grant"})),
        )
            .into_response()
    };
    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return grant();
    }
    let Some(code) = form.get("code") else {
        return grant();
    };
    let Some(pending) = inner.codes.lock().unwrap().remove(code) else {
        return grant();
    };
    if form.get("redirect_uri") != Some(&pending.redirect_uri) {
        return grant();
    }
    let Some(verifier) = form.get("code_verifier") else {
        return grant();
    };
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    if challenge != pending.code_challenge {
        return grant();
    }

    let now = time_now();
    let claims = CoreIdTokenClaims::new(
        IssuerUrl::new(inner.issuer.clone()).unwrap(),
        vec![Audience::new(CLIENT_ID.into())],
        now + chrono::Duration::minutes(5),
        now,
        StandardClaims::new(SubjectIdentifier::new(pending.subject))
            .set_email(pending.email.map(EndUserEmail::new))
            .set_email_verified(pending.email_verified),
        EmptyAdditionalClaims {},
    )
    .set_nonce(Some(Nonce::new(pending.nonce)));
    let id_token = CoreIdToken::new(
        claims,
        &signing_key(),
        CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
        None,
        None,
    )
    .expect("sign id token");
    Json(serde_json::json!({
        "access_token": uuid::Uuid::new_v4().simple().to_string(),
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": id_token.to_string(),
    }))
    .into_response()
}

fn time_now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}
