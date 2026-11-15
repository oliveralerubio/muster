mod support;

use muster_cloud::protocol::{
    error_codes, AccountToken, ApiError, DeviceApproval, DeviceApproveRequest, DeviceAuthorization,
    DeviceAuthorizeRequest, DeviceDecision, DeviceTokenRequest, Workspace, DEVICE_CODE_GRANT_TYPE,
};
use support::{http, stack};

#[tokio::test]
async fn device_flow_rfc8628_authorize_approve_and_token_exchange() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let alice_cookie = format!("__Host-mc_session={}", alice.cookie);
    let client = http();

    // 1. CLI requests authorization without auth: POST /v1/device/authorize
    let auth_req = DeviceAuthorizeRequest {
        client_name: "test-laptop".into(),
    };
    let auth_resp = client
        .post(s.url("/v1/device/authorize"))
        .json(&auth_req)
        .send()
        .await
        .unwrap();

    assert_eq!(auth_resp.status(), 200);
    let auth_data: DeviceAuthorization = auth_resp.json().await.unwrap();
    assert!(!auth_data.device_code.is_empty());
    assert!(!auth_data.user_code.is_empty());
    assert_eq!(auth_data.interval, 5);
    assert_eq!(auth_data.expires_in, 600);
    assert!(auth_data.verification_uri.contains("/device"));
    assert!(auth_data
        .verification_uri_complete
        .contains(&auth_data.user_code));

    // 2. Polling before approval returns 400 authorization_pending
    let token_req = DeviceTokenRequest {
        grant_type: DEVICE_CODE_GRANT_TYPE.into(),
        device_code: auth_data.device_code.clone(),
    };
    let poll_resp = client
        .post(s.url("/v1/device/token"))
        .json(&token_req)
        .send()
        .await
        .unwrap();

    assert_eq!(poll_resp.status(), 400);
    let err: ApiError = poll_resp.json().await.unwrap();
    assert_eq!(err.error.code, error_codes::AUTHORIZATION_PENDING);

    // 3. User approves in web console (session cookie + CSRF): POST /v1/device/approve
    let approve_req = DeviceApproveRequest {
        user_code: auth_data.user_code.clone(),
        decision: DeviceDecision::Approve,
    };
    let approve_resp = client
        .post(s.url("/v1/device/approve"))
        .header("Cookie", &alice_cookie)
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .json(&approve_req)
        .send()
        .await
        .unwrap();

    assert_eq!(approve_resp.status(), 200);
    let app_data: DeviceApproval = approve_resp.json().await.unwrap();
    assert_eq!(app_data.status, "approved");
    assert_eq!(app_data.client_name, "test-laptop");

    // 4. CLI polls token again: returns AccountToken (mca_...)
    let token_resp = client
        .post(s.url("/v1/device/token"))
        .json(&token_req)
        .send()
        .await
        .unwrap();

    assert_eq!(token_resp.status(), 200);
    let token: AccountToken = token_resp.json().await.unwrap();
    assert!(token.access_token.starts_with("mca_"));
    assert_eq!(token.token_type, "Bearer");
    assert_eq!(token.account_id, alice.user_id);
    assert!(token.scope.contains(&"workspaces:read".to_string()));
    assert!(token.scope.contains(&"devices:link".to_string()));

    // 5. Re-polling with redeemed device code fails with 400 invalid_grant
    let repoll_resp = client
        .post(s.url("/v1/device/token"))
        .json(&token_req)
        .send()
        .await
        .unwrap();
    assert_eq!(repoll_resp.status(), 400);
    let repoll_err: ApiError = repoll_resp.json().await.unwrap();
    assert_eq!(repoll_err.error.code, error_codes::INVALID_GRANT);

    // 6. Account token can read workspaces without session cookies: GET /v1/workspaces
    let ws_resp = client
        .get(s.url("/v1/workspaces"))
        .header("Authorization", format!("Bearer {}", token.access_token))
        .send()
        .await
        .unwrap();
    assert_eq!(ws_resp.status(), 200);
    let workspaces: Vec<Workspace> = ws_resp.json().await.unwrap();
    assert_eq!(workspaces.len(), 0);
}

/// Runs authorize + approve(decision) and returns the device code.
async fn authorized_code(
    s: &support::Stack,
    user: &support::UserSession,
    decision: DeviceDecision,
) -> String {
    let client = http();
    let auth: DeviceAuthorization = client
        .post(s.url("/v1/device/authorize"))
        .json(&DeviceAuthorizeRequest {
            client_name: "box".into(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resp = client
        .post(s.url("/v1/device/approve"))
        .header("Cookie", format!("__Host-mc_session={}", user.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &user.csrf_token)
        // Lowercase without dash: codes are case- and dash-insensitive.
        .json(&DeviceApproveRequest {
            user_code: auth.user_code.replace('-', "").to_lowercase(),
            decision,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    auth.device_code
}

async fn poll(s: &support::Stack, device_code: &str) -> reqwest::Response {
    http()
        .post(s.url("/v1/device/token"))
        .json(&DeviceTokenRequest {
            grant_type: DEVICE_CODE_GRANT_TYPE.into(),
            device_code: device_code.into(),
        })
        .send()
        .await
        .unwrap()
}

async fn error_code(resp: reqwest::Response) -> (u16, String, serde_json::Value) {
    let status = resp.status().as_u16();
    let err: ApiError = resp.json().await.unwrap();
    (status, err.error.code, err.error.details)
}

#[tokio::test]
async fn polling_faster_than_interval_is_slowed_down() {
    let s = stack().await;
    let client = http();
    let auth: DeviceAuthorization = client
        .post(s.url("/v1/device/authorize"))
        .json(&DeviceAuthorizeRequest {
            client_name: "box".into(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (_, code, _) = error_code(poll(&s, &auth.device_code).await).await;
    assert_eq!(code, error_codes::AUTHORIZATION_PENDING);
    let (status, code, details) = error_code(poll(&s, &auth.device_code).await).await;
    assert_eq!((status, code.as_str()), (400, error_codes::SLOW_DOWN));
    assert_eq!(details["interval"], 10);
}

#[tokio::test]
async fn denied_login_yields_access_denied_once_then_invalid_grant() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let code = authorized_code(&s, &alice, DeviceDecision::Deny).await;
    let (_, c, _) = error_code(poll(&s, &code).await).await;
    assert_eq!(c, error_codes::ACCESS_DENIED);
    let (_, c, _) = error_code(poll(&s, &code).await).await;
    assert_eq!(c, error_codes::INVALID_GRANT);
}

#[tokio::test]
async fn approving_requires_a_browser_session_with_csrf() {
    let s = stack().await;
    let client = http();
    let auth: DeviceAuthorization = client
        .post(s.url("/v1/device/authorize"))
        .json(&DeviceAuthorizeRequest {
            client_name: "box".into(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let body = DeviceApproveRequest {
        user_code: auth.user_code.clone(),
        decision: DeviceDecision::Approve,
    };
    let resp = client
        .post(s.url("/v1/device/approve"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let alice = s.login_user("alice", "alice@example.com").await;
    let resp = client
        .post(s.url("/v1/device/approve"))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", s.origin())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(error_code(resp).await.1, error_codes::CSRF_FAILED);
}

#[tokio::test]
async fn account_token_is_limited_to_its_scopes_and_revocable() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let code = authorized_code(&s, &alice, DeviceDecision::Approve).await;
    let token: AccountToken = poll(&s, &code).await.json().await.unwrap();
    let bearer = format!("Bearer {}", token.access_token);
    let client = http();

    // No scope for creating workspaces.
    let resp = client
        .post(s.url("/v1/workspaces"))
        .header("Authorization", &bearer)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({"name": "x"}))
        .send()
        .await
        .unwrap();
    let (status, c, details) = error_code(resp).await;
    assert_eq!((status, c.as_str()), (403, error_codes::INSUFFICIENT_SCOPE));
    assert_eq!(details["required_scope"], "workspaces:write");

    // A token is not a browser session: no CSRF token to hand out.
    let resp = client
        .get(s.url("/v1/me"))
        .header("Authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // A wrong-prefix or unknown token is refused.
    let resp = client
        .get(s.url("/v1/workspaces"))
        .header("Authorization", "Bearer mca_unknown")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // CLI logout revokes the token server-side.
    let resp = client
        .post(s.url("/v1/auth/logout"))
        .header("Authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let resp = client
        .get(s.url("/v1/workspaces"))
        .header("Authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}
