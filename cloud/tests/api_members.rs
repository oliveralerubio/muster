mod support;

use muster_cloud::protocol::{
    error_codes, ApiError, CreateInvitation, CreatedInvitation, Invitation, Role, WorkspaceMember,
};
use support::fleet::create_workspace;
use support::{http, stack, Stack, UserSession};

async fn session_get<T: serde::de::DeserializeOwned>(
    s: &Stack,
    u: &UserSession,
    path: &str,
) -> (reqwest::StatusCode, Option<T>) {
    let resp = http()
        .get(s.url(path))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = if status.is_success() {
        Some(resp.json().await.unwrap())
    } else {
        None
    };
    (status, body)
}

async fn session_post(
    s: &Stack,
    u: &UserSession,
    path: &str,
    body: &impl serde::Serialize,
) -> (reqwest::StatusCode, reqwest::Response) {
    let resp = http()
        .post(s.url(path))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &u.csrf_token)
        .json(body)
        .send()
        .await
        .unwrap();
    (resp.status(), resp)
}

async fn session_delete(s: &Stack, u: &UserSession, path: &str) -> reqwest::StatusCode {
    http()
        .delete(s.url(path))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &u.csrf_token)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn member_list_and_invitation_lifecycle() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let bob = s.login_user("bob", "bob@example.com").await;
    let ws = create_workspace(&s, &alice, "Collab").await;

    // 1. Initial members: only the creator (owner).
    let members_path = format!("/v1/workspaces/{}/members", ws.id);
    let (status, list) = session_get::<Vec<WorkspaceMember>>(&s, &alice, &members_path).await;
    assert_eq!(status, 200);
    let list = list.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].email, "alice@example.com");
    assert_eq!(list[0].role, Role::Owner);

    // 2. Alice invites Bob. Clear token is present ONLY in this response.
    let invites_path = format!("/v1/workspaces/{}/invitations", ws.id);
    let (status, resp) = session_post(
        &s,
        &alice,
        &invites_path,
        &CreateInvitation {
            email: "bob@example.com".into(),
            role: Role::Member,
        },
    )
    .await;
    assert_eq!(status, 201);
    let created: CreatedInvitation = resp.json().await.unwrap();
    assert_eq!(created.invitation.email, "bob@example.com");
    assert_eq!(created.invitation.role, Role::Member);
    assert!(!created.token.is_empty());
    assert_eq!(
        created.accept_path,
        format!("/v1/invitations/{}/accept", created.token)
    );

    // 3. GET /invitations: token is NOT returned.
    let (status, inv_list) = session_get::<Vec<Invitation>>(&s, &alice, &invites_path).await;
    assert_eq!(status, 200);
    let inv_list = inv_list.unwrap();
    assert_eq!(inv_list.len(), 1);
    assert_eq!(inv_list[0].email, "bob@example.com");
    assert!(inv_list[0].accepted_at.is_none());

    // 4. Mismatched account (Alice) cannot accept an invitation issued to Bob.
    let accept_path = format!("/v1/invitations/{}/accept", created.token);
    let (status, resp) = session_post(&s, &alice, &accept_path, &serde_json::json!({})).await;
    assert_eq!(status, 403);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::INVITATION_MISMATCH
    );

    // 5. Bob accepts his invitation.
    let (status, _) = session_post(&s, &bob, &accept_path, &serde_json::json!({})).await;
    assert_eq!(status, 200);

    // 6. Bob is now listed as a member.
    let (status, list) = session_get::<Vec<WorkspaceMember>>(&s, &bob, &members_path).await;
    assert_eq!(status, 200);
    let list = list.unwrap();
    assert_eq!(list.len(), 2);
    assert!(list
        .iter()
        .any(|m| m.email == "bob@example.com" && m.role == Role::Member));

    // 7. Re-acceptance fails (single use).
    let (status, _) = session_post(&s, &bob, &accept_path, &serde_json::json!({})).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn role_authorization_and_member_removal() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let bob = s.login_user("bob", "bob@example.com").await;
    let ws = create_workspace(&s, &alice, "Roles").await;

    // Invite and accept Bob as a Member.
    let invites_path = format!("/v1/workspaces/{}/invitations", ws.id);
    let (_, resp) = session_post(
        &s,
        &alice,
        &invites_path,
        &CreateInvitation {
            email: "bob@example.com".into(),
            role: Role::Member,
        },
    )
    .await;
    let created: CreatedInvitation = resp.json().await.unwrap();
    session_post(&s, &bob, &created.accept_path, &serde_json::json!({})).await;

    // Bob (a Member) cannot invite others.
    let (status, resp) = session_post(
        &s,
        &bob,
        &invites_path,
        &CreateInvitation {
            email: "carol@example.com".into(),
            role: Role::Member,
        },
    )
    .await;
    assert_eq!(status, 403);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::FORBIDDEN_ROLE
    );

    // Alice (Owner) removes Bob.
    let member_path = format!("/v1/workspaces/{}/members/{}", ws.id, bob.user_id);
    let status = session_delete(&s, &alice, &member_path).await;
    assert_eq!(status, 204);

    // Bob can no longer access the workspace.
    let (status, _) =
        session_get::<serde_json::Value>(&s, &bob, &format!("/v1/workspaces/{}", ws.id)).await;
    assert_eq!(status, 404);

    // Alice cannot remove herself if she is the last owner.
    let self_path = format!("/v1/workspaces/{}/members/{}", ws.id, alice.user_id);
    let resp = http()
        .delete(s.url(&self_path))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::LAST_OWNER
    );
}

/// Fixture only (precondition, nothing is read back): `user` becomes a
/// second owner of `ws`. The public API never invites an owner, but a
/// workspace with several owners must still never lose its last one.
async fn fixture_make_owner(s: &Stack, ws: &str, user: &str) {
    use sqlx::Connection;
    let url = s
        .env
        .iter()
        .find(|(k, _)| *k == "MUSTER_DATABASE_URL")
        .map(|(_, v)| v.clone())
        .unwrap();
    let mut conn = sqlx::PgConnection::connect(&url).await.unwrap();
    let ws: uuid::Uuid = ws.parse().unwrap();
    let user: uuid::Uuid = user.parse().unwrap();
    let done = sqlx::query(
        "UPDATE memberships SET role = 'owner' WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(ws)
    .bind(user)
    .execute(&mut conn)
    .await
    .unwrap();
    assert_eq!(done.rows_affected(), 1);
}

/// Invites `email` to `ws` as `role` and accepts it as `who`.
async fn join(s: &Stack, by: &UserSession, who: &UserSession, ws: &str, role: Role) {
    let (status, resp) = session_post(
        s,
        by,
        &format!("/v1/workspaces/{ws}/invitations"),
        &CreateInvitation {
            email: who.email.clone(),
            role,
        },
    )
    .await;
    assert_eq!(status, 201);
    let created: CreatedInvitation = resp.json().await.unwrap();
    let (status, _) = session_post(s, who, &created.accept_path, &serde_json::json!({})).await;
    assert_eq!(status, 200);
}

/// Two owners removing each other at the same moment: at most one removal
/// succeeds, and the workspace always keeps an owner who can still manage it.
#[tokio::test]
async fn concurrent_owner_removals_never_leave_a_workspace_without_an_owner() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let bob = s.login_user("bob", "bob@example.com").await;
    for round in 0..12 {
        let ws = create_workspace(&s, &alice, &format!("Owners {round}")).await;
        join(&s, &alice, &bob, &ws.id, Role::Admin).await;
        fixture_make_owner(&s, &ws.id, &bob.user_id).await;

        let remove_bob = format!("/v1/workspaces/{}/members/{}", ws.id, bob.user_id);
        let remove_alice = format!("/v1/workspaces/{}/members/{}", ws.id, alice.user_id);
        let (a, b) = tokio::join!(
            session_delete(&s, &alice, &remove_bob),
            session_delete(&s, &bob, &remove_alice),
        );
        let removed = [a, b].iter().filter(|st| **st == 204).count();
        assert!(
            removed <= 1,
            "round {round}: both owners removed ({a}, {b})"
        );

        let members_path = format!("/v1/workspaces/{}/members", ws.id);
        let mut owners = 0;
        for who in [&alice, &bob] {
            if let (reqwest::StatusCode::OK, Some(list)) =
                session_get::<Vec<WorkspaceMember>>(&s, who, &members_path).await
            {
                owners = list.iter().filter(|m| m.role == Role::Owner).count();
            }
        }
        assert!(owners >= 1, "round {round}: no owner left ({a}, {b})");
    }
}

/// Accepting an invitation never lowers an existing role: the last owner
/// cannot be demoted (leaving the workspace ownerless) through one.
#[tokio::test]
async fn accepting_an_invitation_never_demotes_the_last_owner() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Self invite").await;
    join(&s, &alice, &alice, &ws.id, Role::Member).await;

    let (status, list) = session_get::<Vec<WorkspaceMember>>(
        &s,
        &alice,
        &format!("/v1/workspaces/{}/members", ws.id),
    )
    .await;
    assert_eq!(status, 200);
    let list = list.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].role, Role::Owner, "the only owner was demoted");
}
