//! Rate limits: per authenticated machine on device-signed routes, per
//! client address on the unauthenticated device login flow. Refusals are
//! `429 rate_limited` with `Retry-After`. Preconditions: small limits set
//! through the service's configuration.

mod support;

use muster_cloud::protocol::{
    error_codes, ApiError, DeviceAuthorizeRequest, DeviceTokenRequest, DEVICE_CODE_GRANT_TYPE,
};
use support::fleet::{account_token, create_workspace, link, FleetRepo, MachineKey};
use support::{http, stack_with_env};

async fn assert_rate_limited(resp: reqwest::Response) {
    assert_eq!(resp.status(), 429);
    let retry: u64 = resp.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry), "Retry-After {retry}");
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::RATE_LIMITED
    );
}

/// Each machine has its own budget of signed requests: past it, `429`; a
/// second machine of the same workspace is unaffected.
#[tokio::test]
async fn signed_requests_past_the_per_machine_limit_are_refused() {
    let s = stack_with_env(&[("MUSTER_RATE_LIMIT_DEVICE_PER_MINUTE", "4".into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Rate").await;
    let key_a = MachineKey::new("node-a");
    let key_b = MachineKey::new("node-b");
    let dev_a = link(&s, &token, &ws.id, &key_a).await;
    let dev_b = link(&s, &token, &ws.id, &key_b).await;
    let repo = FleetRepo::genesis(&key_a);
    assert_eq!(
        dev_a
            .send(&s, &dev_a.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );
    for _ in 0..3 {
        let resp = dev_a.send(&s, &dev_a.get(&dev_a.sync_path())).await;
        assert_eq!(resp.status(), 200);
    }
    assert_rate_limited(dev_a.send(&s, &dev_a.get(&dev_a.sync_path())).await).await;
    let resp = dev_b.send(&s, &dev_b.get(&dev_b.sync_path())).await;
    assert_eq!(resp.status(), 200);
}

async fn authorize(s: &support::Stack, forwarded: Option<&str>) -> reqwest::Response {
    let mut req = http()
        .post(s.url("/v1/device/authorize"))
        .json(&DeviceAuthorizeRequest {
            client_name: "laptop".into(),
        });
    if let Some(ip) = forwarded {
        req = req.header("X-Forwarded-For", ip);
    }
    req.send().await.unwrap()
}

/// Link codes (device authorizations) and token polls are limited per
/// client address. Without a trusted proxy, `X-Forwarded-For` is ignored,
/// so a client cannot pick a fresh budget by spoofing it.
#[tokio::test]
async fn the_device_login_flow_is_limited_per_client_address() {
    let s = stack_with_env(&[
        ("MUSTER_RATE_LIMIT_LINK_CODES_PER_MINUTE", "2".into()),
        ("MUSTER_RATE_LIMIT_DEVICE_TOKEN_PER_MINUTE", "2".into()),
    ])
    .await;
    assert_eq!(authorize(&s, Some("203.0.113.1")).await.status(), 200);
    assert_eq!(authorize(&s, Some("203.0.113.2")).await.status(), 200);
    assert_rate_limited(authorize(&s, Some("203.0.113.3")).await).await;

    let poll = || {
        http()
            .post(s.url("/v1/device/token"))
            .json(&DeviceTokenRequest {
                grant_type: DEVICE_CODE_GRANT_TYPE.into(),
                device_code: "dev_unknown".into(),
            })
            .send()
    };
    assert_ne!(poll().await.unwrap().status(), 429);
    assert_ne!(poll().await.unwrap().status(), 429);
    assert_rate_limited(poll().await.unwrap()).await;
}

/// Behind a configured trusted proxy, the client address is the one the
/// proxy forwards: each client has its own budget.
#[tokio::test]
async fn behind_a_trusted_proxy_each_forwarded_client_has_its_own_budget() {
    let s = stack_with_env(&[
        ("MUSTER_RATE_LIMIT_LINK_CODES_PER_MINUTE", "1".into()),
        ("MUSTER_TRUSTED_PROXIES", "127.0.0.1/32,::1".into()),
    ])
    .await;
    assert_eq!(authorize(&s, Some("203.0.113.1")).await.status(), 200);
    assert_rate_limited(authorize(&s, Some("203.0.113.1")).await).await;
    // The rightmost untrusted hop is the client; what it claims before
    // itself is not believed.
    assert_rate_limited(authorize(&s, Some("198.51.100.7, 203.0.113.1")).await).await;
    assert_eq!(authorize(&s, Some("203.0.113.2")).await.status(), 200);
}
