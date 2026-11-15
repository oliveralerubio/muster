//! Account authentication: OIDC login, browser sessions, CLI device flow
//! (RFC 8628) and account tokens.

pub mod account;
pub mod device_flow;
pub mod device_sig;
pub mod oidc;
pub mod session;

use axum::http::HeaderValue;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub use oidc::Oidc;

pub fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

/// 256 random bits, base64url without padding.
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `Set-Cookie` value with the attributes every service cookie carries.
/// `__Host-` cookies require `Secure`, `Path=/` and no `Domain`; browsers
/// treat `http://localhost` as secure, so development works unchanged.
pub fn cookie_header(name: &str, value: &str, max_age_secs: i64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}"
    ))
    .expect("cookie values are base64url")
}

/// Value of cookie `name` from the request's `Cookie` headers.
pub fn read_cookie(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, v)| *k == name && !v.is_empty())
        .map(|(_, v)| v.to_string())
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
