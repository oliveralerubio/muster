//! In-process rate limits (single-VM service): a token bucket per
//! authenticated machine for device-signed routes, and per client address
//! for the unauthenticated device login flow (`POST /v1/device/authorize`,
//! which issues link codes, and `POST /v1/device/token`). A refusal is
//! `429 rate_limited` with `Retry-After` (seconds).
//!
//! Memory is bounded: a bucket idle long enough to be full again is the
//! same as no bucket and is dropped by the next sweep; past a hard cap of
//! keys the table is cleared (briefly failing open) rather than growing.
//!
//! The client address is the TCP peer. Only when that peer is a configured
//! trusted proxy (`MUSTER_TRUSTED_PROXIES`) is `X-Forwarded-For` read: the
//! rightmost hop that is not itself a trusted proxy is the client.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;
use crate::protocol::error_codes;
use crate::state::AppState;

/// Sweep idle buckets once the table holds this many keys.
const SWEEP_AT: usize = 4096;
/// Never hold more keys than this.
const MAX_KEYS: usize = 100_000;

/// Requests per minute; `0` disables a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitConfig {
    /// Per machine, every device-signed request (default 60).
    pub device_per_minute: u32,
    /// Per client address, `POST /v1/device/authorize` (default 10).
    pub link_codes_per_minute: u32,
    /// Per client address, `POST /v1/device/token` (default 60).
    pub device_token_per_minute: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            device_per_minute: 60,
            link_codes_per_minute: 10,
            device_token_per_minute: 60,
        }
    }
}

/// An IP network (`addr/prefix`), for trusted proxies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    /// `203.0.113.4`, `10.0.0.0/8`, `::1`, `fd00::/8`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a.parse::<IpAddr>().ok()?, Some(p.parse::<u8>().ok()?)),
            None => (s.parse::<IpAddr>().ok()?, None),
        };
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        (prefix <= max).then_some(Self { addr, prefix })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, canonical(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// IPv4-mapped IPv6 addresses as IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// Bucket key of a client address: the address itself for IPv4, its /64
/// for IPv6 (one host commonly holds a whole /64).
fn client_key(ip: IpAddr) -> IpAddr {
    match canonical(ip) {
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & (u128::MAX << 64)).into()),
        v4 => v4,
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Token buckets of `per_minute` capacity, refilled at `per_minute / 60`
/// per second.
pub struct Limiter<K> {
    per_minute: u32,
    inner: Mutex<Table<K>>,
}

struct Table<K> {
    buckets: HashMap<K, Bucket>,
    last_sweep: Option<Instant>,
}

impl<K: Hash + Eq + Clone> Limiter<K> {
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            inner: Mutex::new(Table {
                buckets: HashMap::new(),
                last_sweep: None,
            }),
        }
    }

    /// Takes one token for `key`, or says how long until one is available.
    pub fn check(&self, key: &K) -> Result<(), Duration> {
        self.check_at(key, Instant::now())
    }

    fn check_at(&self, key: &K, now: Instant) -> Result<(), Duration> {
        if self.per_minute == 0 {
            return Ok(());
        }
        let capacity = f64::from(self.per_minute);
        let rate = capacity / 60.0;
        let mut table = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Table {
            buckets,
            last_sweep,
        } = &mut *table;
        if !buckets.contains_key(key) && buckets.len() >= SWEEP_AT {
            // At most one sweep a second: idle for a full window means
            // refilled, the same as absent.
            if last_sweep.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1)) {
                *last_sweep = Some(now);
                buckets.retain(|_, b| now.duration_since(b.last) < Duration::from_secs(60));
            }
            if buckets.len() >= MAX_KEYS {
                tracing::warn!(keys = buckets.len(), "rate limit table full; cleared");
                buckets.clear();
            }
        }
        let bucket = buckets.entry(key.clone()).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(capacity);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - bucket.tokens) / rate))
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().buckets.len()
    }
}

/// The service's limiters.
pub struct RateLimits {
    pub device: Limiter<uuid::Uuid>,
    pub link_codes: Limiter<IpAddr>,
    pub device_token: Limiter<IpAddr>,
}

impl RateLimits {
    pub fn new(config: &RateLimitConfig) -> Self {
        Self {
            device: Limiter::new(config.device_per_minute),
            link_codes: Limiter::new(config.link_codes_per_minute),
            device_token: Limiter::new(config.device_token_per_minute),
        }
    }
}

/// `429 rate_limited` with `Retry-After` (whole seconds, at least 1).
pub fn limited(retry: Duration) -> AppError {
    let secs = retry.as_secs_f64().ceil().max(1.0) as u64;
    // The error envelope also sends `retry_after_secs` as `Retry-After`.
    AppError::new(
        StatusCode::TOO_MANY_REQUESTS,
        error_codes::RATE_LIMITED,
        "too many requests; retry later",
    )
    .with_details(serde_json::json!({ "retry_after_secs": secs }))
}

/// The client address of a request: the peer, or what a trusted proxy
/// forwarded (rightmost `X-Forwarded-For` hop that is not trusted itself).
pub fn client_ip(peer: IpAddr, headers: &HeaderMap, trusted: &[IpNet]) -> IpAddr {
    let is_trusted = |ip: IpAddr| trusted.iter().any(|n| n.contains(ip));
    if !is_trusted(peer) {
        return canonical(peer);
    }
    let mut client = canonical(peer);
    let hops = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect::<Vec<_>>();
    for hop in hops.into_iter().rev() {
        let Ok(ip) = hop.parse::<IpAddr>() else {
            break;
        };
        client = canonical(ip);
        if !is_trusted(ip) {
            break;
        }
    }
    client
}

/// Per-address limit on the device login flow's unauthenticated routes.
pub async fn login_flow_layer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let limiter = match req.uri().path() {
        crate::protocol::routes::DEVICE_AUTHORIZE => &state.rate_limits.link_codes,
        crate::protocol::routes::DEVICE_TOKEN => &state.rate_limits.device_token,
        _ => return next.run(req).await,
    };
    // Without connection information (an in-process router) there is no
    // address to limit by.
    let Some(ConnectInfo(peer)) = req.extensions().get::<ConnectInfo<SocketAddr>>().copied() else {
        return next.run(req).await;
    };
    let ip = client_ip(peer.ip(), req.headers(), &state.config.trusted_proxies);
    match limiter.check(&client_key(ip)) {
        Ok(()) => next.run(req).await,
        Err(retry) => limited(retry).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_refills_at_its_rate_and_idle_buckets_are_swept() {
        let limiter = Limiter::<u32>::new(60);
        let t0 = Instant::now();
        for _ in 0..60 {
            assert!(limiter.check_at(&1, t0).is_ok());
        }
        let wait = limiter.check_at(&1, t0).unwrap_err();
        assert!(wait <= Duration::from_secs(1));
        assert!(limiter.check_at(&1, t0 + Duration::from_secs(1)).is_ok());
        for k in 0..SWEEP_AT as u32 {
            let _ = limiter.check_at(&(k + 10), t0);
        }
        let _ = limiter.check_at(&u32::MAX, t0 + Duration::from_secs(61));
        assert!(limiter.len() <= 2);
    }

    #[test]
    fn forwarded_addresses_are_believed_only_from_trusted_proxies() {
        let trusted = [IpNet::parse("10.0.0.0/8").unwrap()];
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4, 10.1.1.1".parse().unwrap());
        let proxy: IpAddr = "10.0.0.1".parse().unwrap();
        let other: IpAddr = "192.0.2.9".parse().unwrap();
        assert_eq!(
            client_ip(proxy, &h, &trusted),
            "1.2.3.4".parse::<IpAddr>().unwrap()
        );
        assert_eq!(client_ip(other, &h, &trusted), other);
        assert_eq!(client_ip(proxy, &HeaderMap::new(), &trusted), proxy);
        assert!(IpNet::parse("10.0.0.0/33").is_none());
        assert!(IpNet::parse("::1")
            .unwrap()
            .contains("::1".parse().unwrap()));
    }
}
