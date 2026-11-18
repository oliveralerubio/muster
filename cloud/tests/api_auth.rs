mod support;

use support::{http, oidc_fixture, set_cookie, stack, stack_with_env, Stack};

#[tokio::test]
async fn login_redirects_to_the_identity_provider_with_pkce() {
    let s = stack().await;
    let resp = http().get(s.url("/auth/login")).send().await.unwrap();
    assert_eq!(resp.status(), 303, "login must redirect to the IdP");

    let location = resp.headers()["location"].to_str().unwrap().to_string();
    let target = url::Url::parse(&location).unwrap();
    assert!(
        location.starts_with(&format!("{}/authorize?", s.idp.issuer)),
        "unexpected redirect {location}"
    );
    let q: std::collections::HashMap<_, _> = target.query_pairs().into_owned().collect();
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], oidc_fixture::CLIENT_ID);
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["redirect_uri"], s.url("/auth/callback"));
    assert!(q["scope"].split(' ').any(|v| v == "openid"));
    for key in ["state", "nonce", "code_challenge"] {
        assert!(q.get(key).is_some_and(|v| v.len() >= 16), "{key} missing");
    }

    let (_, line) = set_cookie(&resp, "__Host-mc_login").expect("login binding cookie");
    for attr in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(line.contains(attr), "cookie lacks {attr}: {line}");
    }
}

#[tokio::test]
async fn callback_exchanges_code_sets_session_cookie_and_identifies_user() {
    let s = stack().await;
    let client = http();

    // 1. Start login
    let login_resp = client.get(s.url("/auth/login")).send().await.unwrap();
    assert_eq!(login_resp.status(), 303);
    let (login_cookie_val, _) = set_cookie(&login_resp, "__Host-mc_login").expect("login cookie");
    let authorize_url = login_resp.headers()["location"].to_str().unwrap();

    // 2. Authorize at IdP as alice
    let idp_auth_url = format!("{authorize_url}&login_as=alice_sub_123&email=alice@example.com");
    let idp_resp = client.get(&idp_auth_url).send().await.unwrap();
    assert_eq!(idp_resp.status(), 303);
    let callback_redirect = idp_resp.headers()["location"].to_str().unwrap();

    // 3. Callback to service with login cookie
    let cb_resp = client
        .get(callback_redirect)
        .header("Cookie", format!("__Host-mc_login={login_cookie_val}"))
        .send()
        .await
        .unwrap();
    assert_eq!(cb_resp.status(), 303, "callback redirects to return_to");
    assert_eq!(cb_resp.headers()["location"], "/");

    let (session_val, session_line) =
        set_cookie(&cb_resp, "__Host-mc_session").expect("session cookie");
    for attr in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(session_line.contains(attr), "session cookie lacks {attr}");
    }

    let (_, cleared_line) = set_cookie(&cb_resp, "__Host-mc_login").expect("cleared login cookie");
    assert!(cleared_line.contains("Max-Age=0"));

    // 4. Authenticated /v1/me
    let me_resp = client
        .get(s.url("/v1/me"))
        .header("Cookie", format!("__Host-mc_session={session_val}"))
        .send()
        .await
        .unwrap();
    assert_eq!(me_resp.status(), 200);
    let me: muster_cloud::protocol::Me = me_resp.json().await.unwrap();
    assert_eq!(me.email, "alice@example.com");
    assert!(!me.id.is_empty());
    assert!(!me.csrf_token.is_empty());

    // 5. Unauthenticated /v1/me returns 401
    let unauth_resp = client.get(s.url("/v1/me")).send().await.unwrap();
    assert_eq!(unauth_resp.status(), 401);
    let err_body: muster_cloud::protocol::ApiError = unauth_resp.json().await.unwrap();
    assert_eq!(err_body.error.code, "unauthenticated");
}

#[tokio::test]
async fn logout_requires_csrf_and_origin_then_invalidates_the_session() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let cookie = format!("__Host-mc_session={}", alice.cookie);
    let client = http();

    // Cross-site attempt: right cookie, no CSRF token, foreign Origin.
    let forged = client
        .post(s.url("/v1/auth/logout"))
        .header("Cookie", &cookie)
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 403);
    let err: muster_cloud::protocol::ApiError = forged.json().await.unwrap();
    assert_eq!(err.error.code, "csrf_failed");

    // Correct token but no Origin header is refused as well.
    let no_origin = client
        .post(s.url("/v1/auth/logout"))
        .header("Cookie", &cookie)
        .header("X-CSRF-Token", &alice.csrf_token)
        .send()
        .await
        .unwrap();
    assert_eq!(no_origin.status(), 403);

    // Session survived both attempts.
    let me = client
        .get(s.url("/v1/me"))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), 200);

    let out = client
        .post(s.url("/v1/auth/logout"))
        .header("Cookie", &cookie)
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .send()
        .await
        .unwrap();
    assert_eq!(out.status(), 204);
    let (_, cleared) = set_cookie(&out, "__Host-mc_session").expect("session cookie cleared");
    assert!(cleared.contains("Max-Age=0"));

    // The old cookie value no longer authenticates (server-side revocation).
    let me = client
        .get(s.url("/v1/me"))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), 401);
}

#[tokio::test]
async fn unverified_or_missing_email_verification_is_refused_at_login() {
    let s = stack().await;
    let client = http();

    for mode in ["false", "missing"] {
        let login_resp = client.get(s.url("/auth/login")).send().await.unwrap();
        assert_eq!(login_resp.status(), 303);
        let (login_cookie, _) = set_cookie(&login_resp, "__Host-mc_login").unwrap();
        let auth_url = format!(
            "{}&login_as=attacker&email=victim@example.com&email_verified={mode}",
            login_resp.headers()["location"].to_str().unwrap()
        );
        let idp_resp = client.get(&auth_url).send().await.unwrap();
        let cb_url = idp_resp.headers()["location"].to_str().unwrap();
        let cb_resp = client
            .get(cb_url)
            .header("Cookie", format!("__Host-mc_login={login_cookie}"))
            .send()
            .await
            .unwrap();

        // Must reject without creating a session: email not verified by IdP.
        assert_eq!(
            cb_resp.status(),
            400,
            "mode {mode} must be refused with 400 bad_request"
        );
        assert!(set_cookie(&cb_resp, "__Host-mc_session").is_none());
        let err: muster_cloud::protocol::ApiError = cb_resp.json().await.unwrap();
        assert_eq!(err.error.code, "bad_request");
    }
}

/// Runs a login whose ID token carries `idp_params` (extra query parameters
/// for the fixture IdP) and returns the callback response.
async fn login_with_token_params(s: &Stack, idp_params: &str) -> reqwest::Response {
    let client = http();
    let login_resp = client.get(s.url("/auth/login")).send().await.unwrap();
    let (login_cookie, _) = set_cookie(&login_resp, "__Host-mc_login").unwrap();
    let auth_url = format!(
        "{}&login_as=zita&email=zita@example.com{idp_params}",
        login_resp.headers()["location"].to_str().unwrap()
    );
    let idp_resp = client.get(&auth_url).send().await.unwrap();
    let cb_url = idp_resp.headers()["location"].to_str().unwrap();
    client
        .get(cb_url)
        .header("Cookie", format!("__Host-mc_login={login_cookie}"))
        .send()
        .await
        .unwrap()
}

async fn assert_login_refused(resp: reqwest::Response) {
    assert_eq!(resp.status(), 400);
    assert!(set_cookie(&resp, "__Host-mc_session").is_none());
    let err: muster_cloud::protocol::ApiError = resp.json().await.unwrap();
    assert_eq!(
        err.error.message,
        "identity provider returned an invalid ID token"
    );
}

#[tokio::test]
async fn configured_extra_audience_is_accepted_with_our_client_as_azp() {
    let s = stack_with_env(&[(
        "MUSTER_OIDC_EXTRA_AUDIENCES",
        " proj-1 , ,proj-2,proj-1".into(),
    )])
    .await;
    let resp = login_with_token_params(&s, "&extra_aud=proj-1&azp=client").await;
    assert_eq!(resp.status(), 303);
    assert!(set_cookie(&resp, "__Host-mc_session").is_some());
}

#[tokio::test]
async fn extra_audience_is_refused_when_not_configured() {
    let s = stack().await;
    assert_login_refused(login_with_token_params(&s, "&extra_aud=proj-1&azp=client").await).await;
}

#[tokio::test]
async fn unlisted_extra_audience_is_refused() {
    let s = stack_with_env(&[("MUSTER_OIDC_EXTRA_AUDIENCES", "proj-1".into())]).await;
    assert_login_refused(login_with_token_params(&s, "&extra_aud=proj-1,other&azp=client").await)
        .await;
    assert_login_refused(login_with_token_params(&s, "&extra_aud=other&azp=client").await).await;
}

#[tokio::test]
async fn multiple_audiences_need_our_client_as_authorized_party() {
    let s = stack_with_env(&[("MUSTER_OIDC_EXTRA_AUDIENCES", "proj-1".into())]).await;
    assert_login_refused(login_with_token_params(&s, "&extra_aud=proj-1").await).await;
    assert_login_refused(login_with_token_params(&s, "&extra_aud=proj-1&azp=proj-1").await).await;
    assert_login_refused(login_with_token_params(&s, "&extra_aud=proj-1&azp=someone-else").await)
        .await;
}

#[tokio::test]
async fn single_audience_token_with_foreign_azp_is_refused() {
    let s = stack().await;
    assert_login_refused(login_with_token_params(&s, "&azp=someone-else").await).await;
}
