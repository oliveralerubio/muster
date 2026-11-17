//! Muster Cloud wire protocol shared by the `muster-cloud` service, the CLI
//! client and the web console.
//!
//! Only plain data lives here: route paths, header names, JSON DTOs, error
//! codes and the canonical byte strings that devices sign. No I/O, no
//! policy. The human-readable map is `API-CONTRACT.md` (kept next to the
//! implementation plan); this module is the normative source for names.
//!
//! Conventions (all routes):
//! - JSON bodies, UTF-8. Lists are JSON arrays; details are JSON objects.
//! - Identifiers are UUID strings (lowercase, hyphenated).
//! - Timestamps are RFC 3339 strings in UTC (`2026-11-13T14:35:00Z`).
//! - Every response carries `X-Request-Id`; every error body is
//!   [`ApiError`].
//!
//! Status of each route (implemented or contract-only) is tracked in
//! `API-CONTRACT.md`; a DTO being defined here does not mean the route is
//! served yet.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Current protocol version, sent by clients in [`HEADER_CLIENT_PROTOCOL`].
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub mod routes {
    //! Route templates. `{ws}`, `{id}` etc. are UUID path segments.
    pub const HEALTHZ: &str = "/healthz";
    pub const READYZ: &str = "/readyz";
    pub const AUTH_LOGIN: &str = "/auth/login";
    pub const AUTH_CALLBACK: &str = "/auth/callback";
    pub const ME: &str = "/v1/me";
    pub const LOGOUT: &str = "/v1/auth/logout";
    pub const WORKSPACES: &str = "/v1/workspaces";
    pub const WORKSPACE: &str = "/v1/workspaces/{ws}";
    /// `GET`: lifecycle of the workspace ([`super::WorkspaceLifecycle`]).
    pub const WORKSPACE_LIFECYCLE: &str = "/v1/workspaces/{ws}/lifecycle";
    pub const DEVICE_AUTHORIZE: &str = "/v1/device/authorize";
    pub const DEVICE_APPROVE: &str = "/v1/device/approve";
    pub const DEVICE_TOKEN: &str = "/v1/device/token";
    pub const WORKSPACE_DEVICES: &str = "/v1/workspaces/{ws}/devices";
    pub const WORKSPACE_SYNC: &str = "/v1/workspaces/{ws}/sync";
    pub const WORKSPACE_RECEIPTS: &str = "/v1/workspaces/{ws}/receipts";
    pub const WORKSPACE_PROPOSALS: &str = "/v1/workspaces/{ws}/proposals";
    pub const WORKSPACE_PROPOSAL: &str = "/v1/workspaces/{ws}/proposals/{id}";
    /// The retained Git bundle of a proposal (`application/x-git-bundle`).
    pub const WORKSPACE_PROPOSAL_BUNDLE: &str = "/v1/workspaces/{ws}/proposals/{id}/bundle";
    /// `POST`: mark a proposal resolved/rejected after local signing.
    pub const WORKSPACE_PROPOSAL_RESOLVE: &str = "/v1/workspaces/{ws}/proposals/{id}/resolve";
    pub const WORKSPACE_MEMBERS: &str = "/v1/workspaces/{ws}/members";
    pub const WORKSPACE_MEMBER: &str = "/v1/workspaces/{ws}/members/{member}";
    pub const WORKSPACE_INVITATIONS: &str = "/v1/workspaces/{ws}/invitations";
    /// Accept an invitation as the authenticated account (token in path).
    pub const INVITATION_ACCEPT: &str = "/v1/invitations/{token}/accept";
    pub const WORKSPACE_REVISIONS: &str = "/v1/workspaces/{ws}/revisions";
    pub const WORKSPACE_REVISION_DIFF: &str = "/v1/workspaces/{ws}/revisions/{rev}/diff";
    /// Full history bundle of the accepted head (`application/x-git-bundle`).
    pub const WORKSPACE_EXPORT: &str = "/v1/workspaces/{ws}/export";
    pub const WORKSPACE_BILLING: &str = "/v1/workspaces/{ws}/billing";
    pub const WORKSPACE_BILLING_CHECKOUT: &str = "/v1/workspaces/{ws}/billing/checkout";
    pub const WORKSPACE_BILLING_PORTAL: &str = "/v1/workspaces/{ws}/billing/portal";
    pub const WORKSPACE_BILLING_CANCEL: &str = "/v1/workspaces/{ws}/billing/cancel";
    pub const WORKSPACE_BILLING_PLAN: &str = "/v1/workspaces/{ws}/billing/plan";
    pub const BILLING_WEBHOOK: &str = "/v1/billing/webhook";
}

// ---------------------------------------------------------------------------
// Headers and cookies
// ---------------------------------------------------------------------------

/// Response header on every response; echoed in error bodies.
pub const HEADER_REQUEST_ID: &str = "x-request-id";
/// Browser mutations: the `csrf_token` returned by `GET /v1/me`.
pub const HEADER_CSRF_TOKEN: &str = "x-csrf-token";
/// Mutations that create resources: a client-generated UUID v4.
pub const HEADER_IDEMPOTENCY_KEY: &str = "idempotency-key";
/// Protocol version the client speaks (decimal, currently `1`).
pub const HEADER_CLIENT_PROTOCOL: &str = "x-muster-protocol";

/// Device-signed requests (sync, receipts, proposals from machines).
/// Authentication is `Authorization: Bearer mcd_...` plus these headers.
pub const HEADER_DEVICE_ID: &str = "x-muster-device";
pub const HEADER_TIMESTAMP: &str = "x-muster-timestamp";
pub const HEADER_NONCE: &str = "x-muster-nonce";
pub const HEADER_CONTENT_SHA256: &str = "x-muster-content-sha256";
pub const HEADER_BASE_REVISION: &str = "x-muster-base-revision";
/// `v1=<lowercase hex Ed25519 signature over canonical_request_v1>`, made
/// with the machine's fleet signing key (`muster::signing::Signer::sign`).
pub const HEADER_SIGNATURE: &str = "x-muster-signature";
/// Sync responses: accepted head served by the service (unverified until the
/// client checks the history itself).
pub const HEADER_REVISION: &str = "x-muster-revision";
pub const HEADER_EPOCH: &str = "x-muster-epoch";

/// Browser session cookie. Always `Secure; HttpOnly; SameSite=Lax; Path=/`.
pub const COOKIE_SESSION: &str = "__Host-mc_session";
/// Short-lived cookie binding an OIDC login attempt to the browser that
/// started it (login CSRF protection). Same attributes as the session.
pub const COOKIE_LOGIN: &str = "__Host-mc_login";

/// `X-Muster-Base-Revision` value when the workspace has no history yet.
pub const BASE_REVISION_NONE: &str = "none";
/// Allowed clock skew for signed requests and receipts, in seconds.
pub const SIGNED_REQUEST_MAX_SKEW_SECS: i64 = 300;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Error body: `{"error":{"code","message","details"},"request_id"}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiError {
    pub error: ApiErrorBody,
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiErrorBody {
    /// Stable machine code, one of [`error_codes`].
    pub code: String,
    /// Human-readable English sentence; not stable, do not parse.
    pub message: String,
    /// Always an object (possibly empty).
    #[serde(default)]
    pub details: serde_json::Value,
}

pub mod error_codes {
    //! Stable error codes and the HTTP status each one is sent with.
    /// 400: malformed JSON, invalid field, missing required header.
    pub const BAD_REQUEST: &str = "bad_request";
    /// 401: no valid session or token.
    pub const UNAUTHENTICATED: &str = "unauthenticated";
    /// 403: browser mutation without a valid `X-CSRF-Token` or `Origin`.
    pub const CSRF_FAILED: &str = "csrf_failed";
    /// 403: authenticated but role does not allow the operation.
    pub const FORBIDDEN_ROLE: &str = "forbidden_role";
    /// 404: unknown route or resource, including another tenant's resource.
    pub const NOT_FOUND: &str = "not_found";
    /// 405: route exists, method does not.
    pub const METHOD_NOT_ALLOWED: &str = "method_not_allowed";
    /// 409: CAS base revision is not the accepted head.
    pub const REVISION_CONFLICT: &str = "revision_conflict";
    /// 422: same `Idempotency-Key` reused for a different request.
    pub const IDEMPOTENCY_MISMATCH: &str = "idempotency_mismatch";
    /// 422: uploaded history fails verification.
    pub const INVALID_HISTORY: &str = "invalid_history";
    /// 402: workspace plan does not allow the operation.
    pub const PLAN_INACTIVE: &str = "plan_inactive";
    /// 413
    pub const PAYLOAD_TOO_LARGE: &str = "payload_too_large";
    /// 426: client protocol too old.
    pub const CLIENT_TOO_OLD: &str = "client_too_old";
    /// 429
    pub const RATE_LIMITED: &str = "rate_limited";
    /// 503: billing provider not configured on this deployment.
    pub const BILLING_UNCONFIGURED: &str = "billing_unconfigured";
    /// 409: the workspace already has a live subscription; change it
    /// (plan/cancel/portal) instead of checking out again.
    pub const SUBSCRIPTION_EXISTS: &str = "subscription_exists";
    /// 503: a dependency (database) is unavailable.
    pub const NOT_READY: &str = "not_ready";
    /// 507
    pub const QUOTA_EXCEEDED: &str = "quota_exceeded";
    /// 500
    pub const INTERNAL: &str = "internal";
    /// 403: the bearer token lacks the scope the route needs
    /// (`details.required_scope`).
    pub const INSUFFICIENT_SCOPE: &str = "insufficient_scope";
    /// 400, `POST /v1/device/token`: the user has not approved yet; poll again
    /// after `interval` seconds.
    pub const AUTHORIZATION_PENDING: &str = "authorization_pending";
    /// 400, `POST /v1/device/token`: polled faster than `interval`; add 5 s to
    /// the interval (`details.interval` carries the new value).
    pub const SLOW_DOWN: &str = "slow_down";
    /// 400, `POST /v1/device/token`: the code expired; start over.
    pub const EXPIRED_TOKEN: &str = "expired_token";
    /// 400, `POST /v1/device/token`: the user denied the request.
    pub const ACCESS_DENIED: &str = "access_denied";
    /// 400, `POST /v1/device/token`: unknown or already redeemed device code,
    /// or wrong `grant_type`.
    pub const INVALID_GRANT: &str = "invalid_grant";
    /// 400, `DELETE /v1/workspaces/{ws}`: `confirm_workspace_id` and
    /// `confirm_workspace_name` are not exactly the workspace's id and name.
    pub const CONFIRMATION_MISMATCH: &str = "confirmation_mismatch";
    /// 409: the workspace has a deletion request; writes, links, proposals,
    /// invitations and billing changes are refused (`details.request_id`,
    /// `details.primary_purge_at`). Pull and export stay available.
    pub const WORKSPACE_DELETION_SCHEDULED: &str = "workspace_deletion_scheduled";
    /// 409: the signing key is already linked in this workspace by another
    /// account.
    pub const DEVICE_CONFLICT: &str = "device_conflict";
    /// 401: device request signature, timestamp, nonce or body digest refused
    /// (`details.reason`).
    pub const INVALID_SIGNATURE: &str = "invalid_signature";
    /// 403: the device is not authorized by the accepted signed governance
    /// for this operation (cloud linking alone never authorizes publishing).
    pub const DEVICE_NOT_APPROVED: &str = "device_not_approved";
    /// 403: the invitation was issued to another email than the
    /// authenticated account's.
    pub const INVITATION_MISMATCH: &str = "invitation_mismatch";
    /// 409: the operation would leave the workspace without an owner.
    pub const LAST_OWNER: &str = "last_owner";
    /// 409: the proposal is not open any more.
    pub const PROPOSAL_CLOSED: &str = "proposal_closed";
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// `GET /healthz` (process alive) and `GET /readyz` (dependencies reachable).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    /// `"ok"` for `/healthz`, `"ready"` for `/readyz`.
    pub status: String,
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

/// `GET /v1/me`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Me {
    pub id: String,
    pub email: String,
    /// Send back as `X-CSRF-Token` on every browser mutation of this session.
    pub csrf_token: String,
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

/// Role of an account inside a workspace. Roles govern the service console
/// and billing; fleet changes are governed by signatures, not roles.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Role::Owner),
            "admin" => Some(Role::Admin),
            "member" => Some(Role::Member),
            _ => None,
        }
    }
}

/// Subscription status shown to clients.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Free trial running (14 days from workspace creation, no card).
    Trialing,
    /// Trial over and no paid subscription.
    TrialExpired,
    Active,
    PastDue,
    Restricted,
    Canceled,
}

/// `POST /v1/workspaces` body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateWorkspace {
    /// 1–64 characters after trimming, no control characters.
    pub name: String,
}

/// `POST /v1/workspaces` (201), `GET /v1/workspaces` (array),
/// `GET /v1/workspaces/{ws}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    /// The caller's role in this workspace.
    pub role: Role,
    pub plan_status: PlanStatus,
    pub trial_ends_at: String,
    pub created_at: String,
    /// Absent from older servers: read as active.
    #[serde(default)]
    pub lifecycle_state: LifecycleState,
}

/// Where a workspace is in its lifecycle.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    #[default]
    Active,
    /// An owner confirmed its deletion: writes are refused, pull and export
    /// stay available until `export_until`.
    DeletionScheduled,
}

/// `DELETE /v1/workspaces/{ws}` body (owner only, with `Idempotency-Key`).
/// Both fields must equal the workspace's id and current name exactly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeleteWorkspaceRequest {
    pub confirm_workspace_id: String,
    pub confirm_workspace_name: String,
}

/// `GET /v1/workspaces/{ws}/lifecycle` (200) and
/// `DELETE /v1/workspaces/{ws}` (202).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceLifecycle {
    pub workspace_id: String,
    pub state: LifecycleState,
    pub deletion: Option<DeletionStatus>,
}

/// The single deletion request of a workspace. Every deadline is fixed when
/// the request is made; retries never move or extend it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeletionStatus {
    pub request_id: String,
    pub requested_at: String,
    /// Pull and export are available until this instant.
    pub export_until: String,
    pub primary: PrimaryPurge,
    pub backups: BackupPurge,
    pub provider: ProviderCancellation,
    pub writes_allowed: bool,
    pub pull_allowed: bool,
    pub export_allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrimaryPurge {
    /// `scheduled` before `purge_at`; `due` after it (the purge may run, it
    /// has not); `purge_committed` once the irreversible tombstone is
    /// recorded (access has ended, erasure in progress); `purged` once the
    /// erasure is verified.
    pub status: String,
    pub purge_at: String,
    /// When the tombstone was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge_committed_at: Option<String>,
    pub purged_at: Option<String>,
    /// Why the purge is not progressing (e.g. live Git access draining).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupPurge {
    /// `unknown` (no catalogue yet, or uncatalogued backups: no deadline is
    /// promised), `none`, `scheduled`, `due`, `overdue`, `error`, `purged`.
    pub status: String,
    pub deadline: Option<String>,
    /// Policy cap for catalogued snapshots (request + 35 days).
    pub cap_at: String,
    pub purged_at: Option<String>,
    pub reason: Option<String>,
    /// Why expired snapshots are not being removed (status `error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderCancellation {
    /// `none` (nothing to cancel), `pending` (some item not yet confirmed
    /// canceled by the provider) or `confirmed`.
    pub status: String,
    pub items: Vec<ProviderCancellationItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderCancellationItem {
    /// `subscription` or `checkout`.
    pub kind: String,
    pub id: String,
    /// `pending` or `confirmed`.
    pub status: String,
    pub confirmed_at: Option<String>,
    pub last_error: Option<String>,
}

/// Maximum workspace name length, in characters.
pub const WORKSPACE_NAME_MAX_CHARS: usize = 64;

// ---------------------------------------------------------------------------
// Members and invitations (cloud roles govern the console, never the fleet)
// ---------------------------------------------------------------------------
//
// Role matrix (account = browser session, or CLI token where the scope says
// so):
// - read members: any member (`workspaces:read`).
// - invite / list invitations / remove others: owner or admin
//   (`workspaces:write`, browser sessions). Only an owner may invite or
//   remove an admin; nobody invites an owner.
// - remove self: any member, except the last owner (`409 last_owner`).
// Removing a member revokes the transport tokens of the machines that
// account linked. It does not change the signed fleet governance: removing
// those machines from the fleet is a locally signed `muster` change.

/// `GET /v1/workspaces/{ws}/members` item (array, oldest first).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceMember {
    /// Account id (same as `Me::id`); path segment of member removal.
    pub user_id: String,
    pub email: String,
    pub role: Role,
    pub joined_at: String,
}

/// `POST /v1/workspaces/{ws}/invitations` body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateInvitation {
    /// Email of the account that may accept (compared case-insensitively
    /// with the email the identity provider reports at acceptance).
    pub email: String,
    /// `admin` or `member` (an owner is never invited).
    pub role: Role,
}

/// `GET /v1/workspaces/{ws}/invitations` item (newest first). Never carries
/// the token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Invitation {
    pub id: String,
    pub email: String,
    pub role: Role,
    pub invited_by: String,
    pub created_at: String,
    pub expires_at: String,
    /// Set once accepted (single use).
    pub accepted_at: Option<String>,
}

/// `POST /v1/workspaces/{ws}/invitations` → `201`. The only response that
/// carries the token: the inviter hands `accept_path` (or the token) to the
/// invitee out of band. The service stores only its hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreatedInvitation {
    pub invitation: Invitation,
    pub token: String,
    /// `/v1/invitations/{token}/accept`.
    pub accept_path: String,
}

/// Invitation lifetime.
pub const INVITATION_TTL_DAYS: i64 = 7;

// ---------------------------------------------------------------------------
// History and export (readable by any member; never gated by plan status)
// ---------------------------------------------------------------------------

/// `GET /v1/workspaces/{ws}/revisions[?before=<rev>&limit=<n>]` item: one
/// accepted head, newest first. `limit` defaults to 50, at most 200;
/// `before` pages to revisions accepted earlier than that one. The Git
/// history between two accepted heads is kept complete (export).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevisionItem {
    pub revision: String,
    /// Previously accepted head (`None` for the first state).
    pub parent_revision: Option<String>,
    pub epoch: u64,
    /// Fleet signing key that sealed the accepted state.
    pub signer: String,
    /// Device that published it.
    pub device_id: String,
    /// Commit subject of the tip.
    pub message: String,
    pub accepted_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffChange {
    Added,
    Modified,
    Deleted,
}

/// One file of [`RevisionDiff`]. Vault ciphertext (`*.age`) never carries
/// content: `patch` is the literal `[ciphertext changed]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffFile {
    pub path: String,
    pub change: DiffChange,
    pub is_vault: bool,
    /// Unified diff text for non-vault files (may be empty for binaries).
    pub patch: String,
}

/// `GET /v1/workspaces/{ws}/revisions/{rev}/diff`: changes of an accepted
/// revision against the previously accepted head (or the empty tree).
/// Unknown or unaccepted revision: `404`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevisionDiff {
    pub revision: String,
    pub base_revision: Option<String>,
    pub files: Vec<DiffFile>,
}

/// `GET /v1/workspaces/{ws}/export` response header: JSON
/// [`ExportManifest`]. The body is a Git bundle with the complete history
/// of `refs/heads/main` at the accepted head; it is verified offline by the
/// open-source CLI (`git clone x.bundle repo && muster sync --expect-signer
/// <key>`), no Cloud subscription needed. Empty workspace: `204`.
pub const HEADER_EXPORT_MANIFEST: &str = "x-muster-export-manifest";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportManifest {
    pub revision: String,
    pub epoch: u64,
    pub signer: String,
    /// Lowercase hex SHA-256 of the bundle body.
    pub sha256: String,
}

// ---------------------------------------------------------------------------
// Device-signed requests
// ---------------------------------------------------------------------------

/// Lowercase hex SHA-256 of a request body (empty body included).
pub fn body_sha256_hex(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Inputs of a version 1 device request signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedRequestV1<'a> {
    /// Uppercase HTTP method.
    pub method: &'a str,
    /// Path plus query exactly as sent on the wire (see
    /// [`canonical_path_and_query`]).
    pub path_and_query: &'a str,
    pub workspace_id: &'a str,
    pub device_id: &'a str,
    /// [`body_sha256_hex`] of the raw body; must equal `X-Muster-Content-SHA256`.
    pub body_sha256_hex: &'a str,
    /// Accepted head the change is based on, or [`BASE_REVISION_NONE`];
    /// empty for requests without CAS.
    pub base_revision: &'a str,
    /// `Idempotency-Key`, or empty when the route has none.
    pub idempotency_key: &'a str,
    /// Unix seconds as a decimal string; must be within
    /// [`SIGNED_REQUEST_MAX_SKEW_SECS`] of the service clock.
    pub timestamp: &'a str,
    /// Fresh random value (≥16 bytes, base64url without padding) per
    /// delivery attempt, including retries; consumed once per device.
    pub nonce: &'a str,
}

const REQUEST_DOMAIN_V1: &str = "muster-cloud-request-v1";

/// Bytes a device signs with its Ed25519 fleet signing key: the fields of
/// [`SignedRequestV1`] in declaration order, each terminated by `\n`, after
/// the domain line. Fields must not contain `\n`; callers reject such input.
pub fn canonical_request_v1(req: &SignedRequestV1<'_>) -> Vec<u8> {
    let fields = [
        REQUEST_DOMAIN_V1,
        req.method,
        req.path_and_query,
        req.workspace_id,
        req.device_id,
        req.body_sha256_hex,
        req.base_revision,
        req.idempotency_key,
        req.timestamp,
        req.nonce,
    ];
    let mut out = Vec::new();
    for field in fields {
        out.extend_from_slice(field.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Canonical form of a path and raw query string: the path unchanged, then,
/// if any query parameters exist, `?` and the `&`-separated pairs sorted
/// byte-wise as raw (still percent-encoded) `name=value` strings.
pub fn canonical_path_and_query(path: &str, raw_query: Option<&str>) -> String {
    let mut pairs: Vec<&str> = raw_query
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty())
        .collect();
    if pairs.is_empty() {
        return path.to_string();
    }
    pairs.sort_unstable();
    format!("{path}?{}", pairs.join("&"))
}

/// Inputs of a version 1 signed apply receipt. `received_at` is assigned by
/// the service and is not signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptV1<'a> {
    pub workspace_id: &'a str,
    pub device_id: &'a str,
    /// Revision (commit SHA) the device applied.
    pub revision: &'a str,
    pub epoch: u64,
    /// Per-device monotonically increasing counter.
    pub sequence: u64,
    /// `applied` or `failed`.
    pub outcome: &'a str,
    /// RFC 3339 time the device applied it (may be old for offline devices).
    pub applied_at: &'a str,
}

const RECEIPT_DOMAIN_V1: &str = "muster-cloud-receipt-v1";

/// Bytes a device signs for a receipt, same framing as
/// [`canonical_request_v1`].
pub fn canonical_receipt_v1(receipt: &ReceiptV1<'_>) -> Vec<u8> {
    let epoch = receipt.epoch.to_string();
    let sequence = receipt.sequence.to_string();
    let fields = [
        RECEIPT_DOMAIN_V1,
        receipt.workspace_id,
        receipt.device_id,
        receipt.revision,
        &epoch,
        &sequence,
        receipt.outcome,
        receipt.applied_at,
    ];
    let mut out = Vec::new();
    for field in fields {
        out.extend_from_slice(field.as_bytes());
        out.push(b'\n');
    }
    out
}

// ---------------------------------------------------------------------------
// CLI login: OAuth 2.0 Device Authorization Grant (RFC 8628)
// ---------------------------------------------------------------------------
//
// Account != machine != signature. The account token obtained here only
// lets the CLI list workspaces and link machines; it never signs fleet
// changes and never authenticates sync (that needs a device token plus a
// request signature by the machine's fleet key).

/// `grant_type` for `POST /v1/device/token`.
pub const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Prefix of account tokens (`Authorization: Bearer mca_...`).
pub const ACCOUNT_TOKEN_PREFIX: &str = "mca_";
/// Prefix of device (machine transport) tokens (`Authorization: Bearer mcd_...`).
pub const DEVICE_TOKEN_PREFIX: &str = "mcd_";

pub mod scopes {
    //! Explicit permissions carried by an account token. Every route that
    //! accepts an account token names the one scope it needs.
    /// `GET /v1/workspaces`, `GET /v1/workspaces/{ws}`,
    /// `GET /v1/workspaces/{ws}/devices`.
    pub const WORKSPACES_READ: &str = "workspaces:read";
    /// `POST /v1/workspaces/{ws}/devices`.
    pub const DEVICES_LINK: &str = "devices:link";
    /// `POST /v1/workspaces`, invitations, member removal, proposal
    /// resolution by account. Browser sessions only: never granted to CLI
    /// tokens.
    pub const WORKSPACES_WRITE: &str = "workspaces:write";
    /// What a CLI login grants (all of the above, nothing else).
    pub const CLI_LOGIN: &[&str] = &[WORKSPACES_READ, DEVICES_LINK];
}

/// `POST /v1/device/authorize` body (no authentication).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceAuthorizeRequest {
    /// Shown to the user on the approval page (e.g. the hostname); 1–64
    /// characters without control characters.
    pub client_name: String,
}

/// `POST /v1/device/authorize` response (200).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceAuthorization {
    /// Secret; only the CLI keeps it, to poll `POST /v1/device/token`.
    pub device_code: String,
    /// `XXXX-XXXX` (A–Z without vowels-like ambiguity, 2–9); the user
    /// types or confirms it in the browser.
    pub user_code: String,
    /// Web page where a signed-in user approves the code.
    pub verification_uri: String,
    /// `verification_uri` with `?user_code=...`.
    pub verification_uri_complete: String,
    /// Seconds until both codes expire (600).
    pub expires_in: u64,
    /// Minimum seconds between polls (5).
    pub interval: u64,
}

/// Decision of the signed-in user on a pending CLI login.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceDecision {
    Approve,
    Deny,
}

/// `POST /v1/device/approve` body (browser session + CSRF).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceApproveRequest {
    /// Case-insensitive; the dash is optional.
    pub user_code: String,
    pub decision: DeviceDecision,
}

/// `POST /v1/device/approve` response (200).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceApproval {
    /// `approved` or `denied`.
    pub status: String,
    pub client_name: String,
}

/// `POST /v1/device/token` body. Errors use the standard envelope with
/// codes `authorization_pending`, `slow_down`, `expired_token`,
/// `access_denied`, `invalid_grant` (all HTTP 400).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceTokenRequest {
    /// Must be [`DEVICE_CODE_GRANT_TYPE`].
    pub grant_type: String,
    pub device_code: String,
}

/// `POST /v1/device/token` success (200). A device code is redeemed once.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountToken {
    /// `mca_...`; store 0600, never in argv, URLs or logs.
    pub access_token: String,
    /// Always `Bearer`.
    pub token_type: String,
    /// Seconds of validity (30 days).
    pub expires_in: u64,
    /// Exactly [`scopes::CLI_LOGIN`].
    pub scope: Vec<String>,
    /// The account the token acts for (same id as `GET /v1/me`).
    pub account_id: String,
}

// ---------------------------------------------------------------------------
// Machines (cloud devices) linked to a workspace
// ---------------------------------------------------------------------------

/// Transport status of a machine in a workspace. Derived by the service
/// from the accepted signed governance, never from cloud roles:
/// - `linked`: has a transport token; its key is not authorized by the
///   accepted governance and it has no open proposal.
/// - `pending`: has an open proposal (e.g. its enrollment) waiting for the
///   fleet's admins.
/// - `approved`: the accepted governance authorizes its signing key.
/// - `revoked`: its key was authorized by an accepted state and no longer
///   is (or is listed as revoked).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceStatus {
    Linked,
    Pending,
    Approved,
    Revoked,
}

/// Accepted head of a workspace as recorded by the service. Unverified for
/// the client until it checks the history against its own pin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeadRef {
    /// Commit SHA (40 lowercase hex).
    pub revision: String,
    pub epoch: u64,
}

/// `POST /v1/workspaces/{ws}/devices` body. Auth: account token with
/// `devices:link` (or browser session + CSRF), plus `Idempotency-Key`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkDeviceRequest {
    /// Machine name (1–64 of `[A-Za-z0-9._-]`).
    pub machine_name: String,
    /// `musterpk1<hex>`: the machine's fleet signing key.
    pub signing_key: String,
    /// `age1...`: the machine's age recipient.
    pub recipient: String,
    /// Proof of possession: hex signature by `signing_key` over
    /// [`canonical_link_v1`].
    pub proof: String,
}

/// `POST /v1/workspaces/{ws}/devices` response (201). Re-linking the same key
/// by the same account rotates the token and keeps `device_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkedDevice {
    pub device_id: String,
    pub workspace_id: String,
    /// `mcd_...`; store 0600. Shown once.
    pub device_token: String,
    pub status: DeviceStatus,
    /// This machine holds the workspace's bootstrap claim: the workspace has
    /// no history and the first signed state may come only from it (and only
    /// if that state's own governance authorizes its key). Claimed
    /// atomically by the first machine an owner links to an empty workspace.
    pub bootstrap: bool,
    pub head: Option<HeadRef>,
}

/// `GET /v1/workspaces/{ws}/devices` item (array, oldest first).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub machine_name: String,
    pub signing_key: String,
    pub recipient: String,
    pub status: DeviceStatus,
    pub bootstrap: bool,
    /// Account that linked it.
    pub linked_by: String,
    pub created_at: String,
    /// Last signed request accepted from it.
    pub last_seen_at: Option<String>,
    /// Newest receipt by sequence, if any.
    pub last_receipt: Option<Receipt>,
}

const LINK_DOMAIN_V1: &str = "muster-cloud-link-v1";

/// Bytes signed for [`LinkDeviceRequest::proof`]: domain, workspace id,
/// account id ([`AccountToken::account_id`] or `GET /v1/me`), machine name,
/// signing key and recipient, each terminated by `\n`.
pub fn canonical_link_v1(
    workspace_id: &str,
    account_id: &str,
    machine_name: &str,
    signing_key: &str,
    recipient: &str,
) -> Vec<u8> {
    frame(&[
        LINK_DOMAIN_V1,
        workspace_id,
        account_id,
        machine_name,
        signing_key,
        recipient,
    ])
}

// ---------------------------------------------------------------------------
// Sync (Git bundle transport with CAS)
// ---------------------------------------------------------------------------
//
// `GET /v1/workspaces/{ws}/sync[?since=<rev>]` (device-signed, empty body,
// empty base and idempotency key in the signature):
//   200 `application/x-git-bundle` containing `refs/heads/main` at the
//       accepted head (full history, or `since..head` when `since` is an
//       ancestor of head), headers `X-Muster-Revision`, `X-Muster-Epoch`;
//   204 when the workspace has no head yet or `since` is the head.
//
// `POST /v1/workspaces/{ws}/sync` (device-signed, body = Git bundle whose only
// ref is `refs/heads/main`, prerequisites must be the base revision):
//   200 [`SyncAccepted`]: the new head;
//   202 [`SyncHeld`]: valid history whose newest state is pending quorum,
//       or a proposal from a machine the governance does not authorize yet;
//       retained as a proposal, the head is unchanged;
//   409 `revision_conflict`, `details` = [`ConflictDetails`]: base is not
//       the head; the bundle is retained as a proposal, never lost;
//   422 `invalid_history` (`details.reason`), 403 `device_not_approved`.

/// Media type of sync bodies.
pub const GIT_BUNDLE_MEDIA_TYPE: &str = "application/x-git-bundle";
/// The only ref a sync bundle may carry.
pub const SYNC_REF: &str = "refs/heads/main";

/// `POST /sync` 200.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncAccepted {
    pub revision: String,
    pub epoch: u64,
}

/// `POST /sync` 202.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncHeld {
    pub proposal_id: String,
    pub status: ProposalStatus,
    pub kind: ProposalKind,
    /// The unchanged accepted head.
    pub head: Option<HeadRef>,
}

/// `details` of a 409 `revision_conflict`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictDetails {
    /// Current accepted head (`none` when empty).
    pub head: String,
    pub epoch: u64,
    /// The retained proposal holding the refused bundle.
    pub proposal_id: String,
}

// ---------------------------------------------------------------------------
// Proposals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    /// Based on a stale head (CAS lost); rebase/re-seal locally.
    Conflict,
    /// Newest state is a governance change short of its admin quorum.
    PendingQuorum,
    /// From a machine the accepted governance does not authorize (e.g. its
    /// own enrollment request pushed as a bundle).
    Unauthorized,
    /// A linked machine requesting enrollment into the fleet.
    Enrollment,
    /// Request to restore files from an earlier accepted revision.
    Restore,
}

impl ProposalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProposalKind::Conflict => "conflict",
            ProposalKind::PendingQuorum => "pending_quorum",
            ProposalKind::Unauthorized => "unauthorized",
            ProposalKind::Enrollment => "enrollment",
            ProposalKind::Restore => "restore",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Open,
    Resolved,
    Rejected,
    Superseded,
}

impl ProposalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProposalStatus::Open => "open",
            ProposalStatus::Resolved => "resolved",
            ProposalStatus::Rejected => "rejected",
            ProposalStatus::Superseded => "superseded",
        }
    }
}

/// `GET /v1/workspaces/{ws}/proposals` item (array, newest first) and
/// `GET /v1/workspaces/{ws}/proposals/{id}`. Readable by any member (browser
/// session) or by any device of the workspace (signed request).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Proposal {
    pub id: String,
    pub kind: ProposalKind,
    pub status: ProposalStatus,
    /// Base revision the proposer claimed (`none` when empty).
    pub base_revision: String,
    /// Tip commit of the retained bundle (empty when no bundle).
    pub revision: String,
    /// Epoch of the proposed newest state, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
}

/// `POST /v1/workspaces/{ws}/proposals` body for declarative requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateProposal {
    pub kind: ProposalKind,
    pub base_revision: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// `POST /v1/workspaces/{ws}/proposals/{id}/resolve` body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolveProposal {
    /// Accepted revision that incorporates or replaces this proposal.
    /// `None` marks the proposal rejected without incorporating it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_revision: Option<String>,
}

/// Payload of a `restore` proposal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestorePayload {
    pub target_revision: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

// ---------------------------------------------------------------------------
// Receipts
// ---------------------------------------------------------------------------

/// Applied or failed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptOutcome {
    Applied,
    Failed,
}

impl ReceiptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            ReceiptOutcome::Applied => "applied",
            ReceiptOutcome::Failed => "failed",
        }
    }
}

/// `POST /v1/workspaces/{ws}/receipts` body (device-signed request; the
/// receipt itself is also signed so it stays verifiable as history).
/// Receipts attest what the machine declares, not what its agents did.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmitReceipt {
    pub revision: String,
    pub epoch: u64,
    /// Per-device counter, strictly positive. An older sequence delivered
    /// late is stored as history without replacing a newer one; the same
    /// receipt again is idempotent; a different receipt with a used
    /// sequence is `422 idempotency_mismatch`.
    pub sequence: u64,
    pub outcome: ReceiptOutcome,
    /// RFC 3339; when the machine applied it (may be old: offline).
    pub applied_at: String,
    /// Hex signature by the device's signing key over
    /// [`canonical_receipt_v1`].
    pub signature: String,
}

/// Stored receipt (`POST /receipts` 201/200, `Device::last_receipt`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Receipt {
    pub device_id: String,
    pub revision: String,
    pub epoch: u64,
    pub sequence: u64,
    pub outcome: ReceiptOutcome,
    pub applied_at: String,
    /// Assigned by the service on first delivery.
    pub received_at: String,
    pub signature: String,
}

fn frame(fields: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for field in fields {
        out.extend_from_slice(field.as_bytes());
        out.push(b'\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Billing and entitlements (Paddle Billing)
// ---------------------------------------------------------------------------

/// `GET /v1/workspaces/{ws}/billing` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceBilling {
    pub workspace_id: String,
    pub billing_configured: bool,
    /// `trial`, `personal` or `team`.
    pub plan: String,
    pub plan_status: PlanStatus,
    pub seats: u32,
    pub trial_ends_at: Option<String>,
    pub current_period_end: Option<String>,
    pub cancel_at_period_end: bool,
    pub can_push: bool,
    pub can_pull: bool,
    pub can_export: bool,
    pub max_devices: u32,
    pub max_bundle_bytes: usize,
    pub max_storage_bytes: u64,
    /// Storage the workspace consumes against `max_storage_bytes`: every Git
    /// object the service keeps (accepted history, retained proposals and
    /// conflicts, refused-but-stored uploads), ciphertext included.
    #[serde(default)]
    pub storage_used_bytes: u64,
}

fn default_one() -> u32 {
    1
}

/// `POST /v1/workspaces/{ws}/billing/checkout` body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateCheckout {
    /// `personal` or `team`.
    pub plan: String,
    /// Number of seats (1 for personal; >= 1 for team).
    #[serde(default = "default_one")]
    pub seats: u32,
}

/// `POST /v1/workspaces/{ws}/billing/checkout` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckoutSession {
    pub checkout_url: String,
    pub transaction_id: String,
}

/// `POST /v1/workspaces/{ws}/billing/portal` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomerPortalSession {
    pub portal_url: String,
}

/// `POST /v1/workspaces/{ws}/billing/plan` body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdatePlan {
    pub plan: String,
    pub seats: u32,
}
