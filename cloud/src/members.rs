//! Memberships and invitations.
//!
//! Cloud roles (owner, admin, member) govern the web console, invitations,
//! and member removal. They never grant fleet authority (ADR-0005).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use sqlx::PgPool;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::account::Account;
use crate::auth::random_token;
use crate::auth::sha256;
use crate::error::AppError;
use crate::protocol::{
    error_codes, scopes, CreateInvitation, CreatedInvitation, Invitation, Role, WorkspaceMember,
    INVITATION_TTL_DAYS,
};
use crate::state::AppState;
use crate::tenancy::parse_json;

fn rfc3339(t: OffsetDateTime) -> Result<String, AppError> {
    t.format(&Rfc3339).map_err(AppError::internal)
}

fn parse_uuid(s: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(s).map_err(|_| AppError::not_found())
}

async fn caller_role(db: &PgPool, ws: Uuid, user: Uuid) -> Result<Role, AppError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2")
            .bind(ws)
            .bind(user)
            .fetch_optional(db)
            .await?;
    let (role_str,) = row.ok_or_else(AppError::not_found)?;
    Role::parse(&role_str).ok_or_else(|| AppError::internal("corrupt role in db"))
}

/// Order of authority of a Cloud role (higher is more).
fn rank(role: Role) -> u8 {
    match role {
        Role::Member => 0,
        Role::Admin => 1,
        Role::Owner => 2,
    }
}

/// `GET /v1/workspaces/{ws}/members`: all members of the workspace, oldest first.
pub async fn list_members(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
) -> Result<Json<Vec<WorkspaceMember>>, AppError> {
    account.require_scope(scopes::WORKSPACES_READ)?;
    let ws = parse_uuid(&ws_str)?;
    let _ = caller_role(&state.db, ws, account.user_id).await?;

    let rows: Vec<(Uuid, String, String, OffsetDateTime)> = sqlx::query_as(
        "SELECT m.user_id, u.email, m.role, m.created_at
         FROM memberships m
         JOIN users u ON u.id = m.user_id
         WHERE m.workspace_id = $1
         ORDER BY m.created_at ASC, m.user_id ASC",
    )
    .bind(ws)
    .fetch_all(&state.db)
    .await?;

    let members = rows
        .into_iter()
        .map(|(uid, email, role_str, joined_at)| {
            let role = Role::parse(&role_str).unwrap_or(Role::Member);
            Ok(WorkspaceMember {
                user_id: uid.to_string(),
                email,
                role,
                joined_at: rfc3339(joined_at)?,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;

    Ok(Json(members))
}

/// `POST /v1/workspaces/{ws}/invitations`: create a single-use invitation.
pub async fn create_invitation(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<CreatedInvitation>), AppError> {
    account.require_scope(scopes::WORKSPACES_WRITE)?;
    let ws = parse_uuid(&ws_str)?;
    let caller = caller_role(&state.db, ws, account.user_id).await?;

    let req: CreateInvitation = parse_json(&body)?;
    let email = req.email.trim().to_lowercase();
    if !email.contains('@') || email.len() < 3 || email.len() > 255 {
        return Err(AppError::bad_request("invalid email"));
    }

    // Role permissions:
    // Member cannot invite anyone.
    // Admin can only invite Member.
    // Nobody can invite Owner.
    match (caller, req.role) {
        (Role::Member, _) => {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                error_codes::FORBIDDEN_ROLE,
                "members cannot invite others",
            ))
        }
        (Role::Admin, Role::Admin | Role::Owner) => {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                error_codes::FORBIDDEN_ROLE,
                "admins can only invite members",
            ))
        }
        (Role::Owner, Role::Owner) => {
            return Err(AppError::bad_request("cannot invite an owner"));
        }
        _ => {}
    }

    let token = random_token();
    let token_hash = sha256(token.as_bytes());
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let expires_at = now + time::Duration::days(INVITATION_TTL_DAYS);

    let mut tx = state.db.begin().await?;
    crate::lifecycle::lock_writable(&mut tx, ws, false).await?;
    sqlx::query(
        "INSERT INTO invitations (id, workspace_id, email, role, token_hash, invited_by, created_at, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(ws)
    .bind(&email)
    .bind(req.role.as_str())
    .bind(&token_hash)
    .bind(account.user_id)
    .bind(now)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let created = CreatedInvitation {
        invitation: Invitation {
            id: id.to_string(),
            email,
            role: req.role,
            invited_by: account.user_id.to_string(),
            created_at: rfc3339(now)?,
            expires_at: rfc3339(expires_at)?,
            accepted_at: None,
        },
        token: token.clone(),
        accept_path: format!("/v1/invitations/{token}/accept"),
    };

    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /v1/workspaces/{ws}/invitations`: open and historical invitations.
pub async fn list_invitations(
    State(state): State<AppState>,
    account: Account,
    Path(ws_str): Path<String>,
) -> Result<Json<Vec<Invitation>>, AppError> {
    account.require_scope(scopes::WORKSPACES_READ)?;
    let ws = parse_uuid(&ws_str)?;
    let caller = caller_role(&state.db, ws, account.user_id).await?;
    if caller == Role::Member {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            error_codes::FORBIDDEN_ROLE,
            "members cannot list invitations",
        ));
    }

    type InvitationRow = (
        Uuid,
        String,
        String,
        Uuid,
        OffsetDateTime,
        OffsetDateTime,
        Option<OffsetDateTime>,
    );
    let rows: Vec<InvitationRow> = sqlx::query_as(
        "SELECT id, email, role, invited_by, created_at, expires_at, accepted_at
         FROM invitations
         WHERE workspace_id = $1
         ORDER BY created_at DESC",
    )
    .bind(ws)
    .fetch_all(&state.db)
    .await?;

    let list = rows
        .into_iter()
        .map(
            |(id, email, role_str, inv_by, created, expires, accepted)| {
                let role = Role::parse(&role_str).unwrap_or(Role::Member);
                Ok(Invitation {
                    id: id.to_string(),
                    email,
                    role,
                    invited_by: inv_by.to_string(),
                    created_at: rfc3339(created)?,
                    expires_at: rfc3339(expires)?,
                    accepted_at: accepted.map(rfc3339).transpose()?,
                })
            },
        )
        .collect::<Result<Vec<_>, AppError>>()?;

    Ok(Json(list))
}

/// `POST /v1/invitations/{token}/accept`: accepts an invitation as the caller.
pub async fn accept_invitation(
    State(state): State<AppState>,
    account: Account,
    Path(token): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let token_hash = sha256(token.as_bytes());

    let user_row: Option<(String,)> = sqlx::query_as("SELECT email FROM users WHERE id = $1")
        .bind(account.user_id)
        .fetch_optional(&state.db)
        .await?;
    let (caller_email,) = user_row.ok_or_else(AppError::unauthenticated)?;

    let mut tx = state.db.begin().await?;

    type InvRow = (
        Uuid,
        Uuid,
        String,
        String,
        OffsetDateTime,
        Option<OffsetDateTime>,
    );
    let inv_row: Option<InvRow> = sqlx::query_as(
        "SELECT id, workspace_id, email, role, expires_at, accepted_at
         FROM invitations WHERE token_hash = $1 FOR UPDATE",
    )
    .bind(&token_hash)
    .fetch_optional(&mut *tx)
    .await?;

    let (inv_id, ws_id, target_email, role_str, expires_at, accepted_at) =
        inv_row.ok_or_else(AppError::not_found)?;

    if accepted_at.is_some() || expires_at < OffsetDateTime::now_utc() {
        return Err(AppError::not_found());
    }

    if !caller_email
        .trim()
        .eq_ignore_ascii_case(target_email.trim())
    {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            error_codes::INVITATION_MISMATCH,
            "invitation was issued to another email",
        ));
    }

    crate::lifecycle::lock_writable(&mut tx, ws_id, false).await?;
    let invited = Role::parse(&role_str).unwrap_or(Role::Member);
    // Accepting never lowers a role the caller already holds: an owner
    // accepting a member invitation stays owner, so this path can never
    // take a workspace's owner count down (to zero, say).
    let current: Option<String> = sqlx::query_scalar(
        "SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2 FOR UPDATE",
    )
    .bind(ws_id)
    .bind(account.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let role = match current.as_deref().and_then(Role::parse) {
        Some(held) if rank(held) >= rank(invited) => held,
        _ => invited,
    };
    sqlx::query(
        "INSERT INTO memberships (workspace_id, user_id, role)
         VALUES ($1, $2, $3)
         ON CONFLICT (workspace_id, user_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(ws_id)
    .bind(account.user_id)
    .bind(role.as_str())
    .execute(&mut *tx)
    .await?;

    sqlx::query("UPDATE invitations SET accepted_at = now(), accepted_by = $1 WHERE id = $2")
        .bind(account.user_id)
        .bind(inv_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "workspace_id": ws_id.to_string(),
        "role": role.as_str(),
    })))
}

/// `DELETE /v1/workspaces/{ws}/members/{member}`: removes a member.
pub async fn remove_member(
    State(state): State<AppState>,
    account: Account,
    Path((ws_str, member_str)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    account.require_scope(scopes::WORKSPACES_WRITE)?;
    let ws = parse_uuid(&ws_str)?;
    let target = parse_uuid(&member_str)?;
    caller_role(&state.db, ws, account.user_id).await?;

    let mut tx = state.db.begin().await?;
    // Every removal in a workspace is serialised on its row (as a deletion
    // request is): the owner count and the caller's role below are read
    // after any concurrent removal committed, so two owners removing each
    // other can never both succeed and leave the workspace without one.
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(ws)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(AppError::not_found)?;
    let caller: Option<String> =
        sqlx::query_scalar("SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2")
            .bind(ws)
            .bind(account.user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let caller = caller
        .as_deref()
        .map(|r| Role::parse(r).ok_or_else(|| AppError::internal("corrupt role in db")))
        .transpose()?
        .ok_or_else(AppError::not_found)?;

    let target_row: Option<(String,)> = sqlx::query_as(
        "SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2 FOR UPDATE",
    )
    .bind(ws)
    .bind(target)
    .fetch_optional(&mut *tx)
    .await?;

    let (target_role_str,) = target_row.ok_or_else(AppError::not_found)?;
    let target_role = Role::parse(&target_role_str).unwrap_or(Role::Member);

    // Permission rules:
    // A Member can only remove themselves.
    // An Admin can remove Members, but cannot remove Owners or other Admins.
    // An Owner can remove anyone except the last remaining Owner.
    if account.user_id != target {
        match caller {
            Role::Member => {
                return Err(AppError::new(
                    StatusCode::FORBIDDEN,
                    error_codes::FORBIDDEN_ROLE,
                    "members cannot remove other members",
                ))
            }
            Role::Admin if target_role != Role::Member => {
                return Err(AppError::new(
                    StatusCode::FORBIDDEN,
                    error_codes::FORBIDDEN_ROLE,
                    "admins cannot remove owners or admins",
                ))
            }
            _ => {}
        }
    }

    if target_role == Role::Owner {
        let (owner_count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM memberships WHERE workspace_id = $1 AND role = 'owner'",
        )
        .bind(ws)
        .fetch_one(&mut *tx)
        .await?;
        if owner_count <= 1 {
            return Err(AppError::new(
                StatusCode::CONFLICT,
                error_codes::LAST_OWNER,
                "cannot remove the last owner of the workspace",
            ));
        }
    }

    sqlx::query("DELETE FROM memberships WHERE workspace_id = $1 AND user_id = $2")
        .bind(ws)
        .bind(target)
        .execute(&mut *tx)
        .await?;

    // Revoke devices linked by the removed member in this workspace.
    sqlx::query("UPDATE devices SET status = 'revoked' WHERE workspace_id = $1 AND linked_by = $2")
        .bind(ws)
        .bind(target)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(StatusCode::NO_CONTENT)
}
