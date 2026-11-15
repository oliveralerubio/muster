//! Client-side fixtures for sync tests: real signed fleet repositories built
//! with the `muster` library (the code the CLI runs), Git bundles made with
//! the `git` binary, and device-signed HTTP requests per protocol v1.
//!
//! Nothing here talks to the service except through HTTP.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use muster::fleet_state::seal;
use muster::manifest::{Machine, Manifest, Member, MemberStatus, Role, TeamConfig};
use muster::signing::Signer;
use muster_cloud::protocol::{
    body_sha256_hex, canonical_link_v1, canonical_request_v1, AccountToken, CreateWorkspace,
    DeviceApproveRequest, DeviceAuthorization, DeviceAuthorizeRequest, DeviceDecision,
    DeviceTokenRequest, LinkDeviceRequest, LinkedDevice, SignedRequestV1, Workspace,
    BASE_REVISION_NONE, DEVICE_CODE_GRANT_TYPE, HEADER_BASE_REVISION, HEADER_CONTENT_SHA256,
    HEADER_DEVICE_ID, HEADER_IDEMPOTENCY_KEY, HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP,
};
use tempfile::TempDir;

use super::{http, Stack, UserSession};

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A machine: its signing key (state dir) and age recipient.
pub struct MachineKey {
    _dir: TempDir,
    pub name: String,
    pub signer: Signer,
    pub recipient: String,
}

impl MachineKey {
    pub fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let signer = Signer::load_or_create(dir.path()).unwrap();
        Self {
            _dir: dir,
            name: name.into(),
            signer,
            recipient: format!("age1{name}synthetic"),
        }
    }

    pub fn public(&self) -> String {
        self.signer.public()
    }

    fn entry(&self) -> Machine {
        Machine {
            recipient: self.recipient.clone(),
            signing_key: Some(self.public()),
            platform: "linux-x86_64".into(),
            muster_version: "0.1.0".into(),
            agents: vec![],
            last_sync: None,
            host: None,
            role: None,
            profile: None,
            applied_profile: None,
        }
    }
}

/// A local config repository of one machine (its own pin in `state`).
pub struct FleetRepo {
    _dir: TempDir,
    pub config: PathBuf,
    pub state: PathBuf,
    pub manifest: Manifest,
}

impl FleetRepo {
    fn empty() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let state = dir.path().join("state");
        fs::create_dir_all(config.join("vault")).unwrap();
        fs::create_dir_all(&state).unwrap();
        git(&config, &["init", "-q", "-b", "main"]);
        git(&config, &["config", "user.name", "Test"]);
        git(&config, &["config", "user.email", "test@example.com"]);
        Self {
            _dir: dir,
            config,
            state,
            manifest: Manifest::new(),
        }
    }

    /// Genesis (epoch 1) signed by `machine`, whose own governance lists it.
    pub fn genesis(machine: &MachineKey) -> Self {
        let mut repo = Self::empty();
        repo.manifest
            .machines
            .insert(machine.name.clone(), machine.entry());
        fs::write(
            repo.config.join("vault/default.age"),
            b"synthetic-ciphertext",
        )
        .unwrap();
        repo.seal_commit(machine, "genesis");
        repo
    }

    /// Team genesis signed by `a`: members alice (admin, `a`) and bob
    /// (admin, `b`), two admin approvals required for governance changes.
    pub fn team_genesis(a: &MachineKey, b: &MachineKey) -> Self {
        let mut repo = Self::empty();
        for (member, key) in [("alice", a), ("bob", b)] {
            repo.manifest.machines.insert(key.name.clone(), key.entry());
            repo.manifest.members.insert(
                member.into(),
                Member {
                    role: Role::Admin,
                    status: MemberStatus::Active,
                    machines: vec![key.name.clone()],
                    ..Member::default()
                },
            );
        }
        repo.manifest.team = Some(TeamConfig {
            required_approvals: 2,
        });
        fs::write(
            repo.config.join("vault/default.age"),
            b"synthetic-ciphertext",
        )
        .unwrap();
        repo.seal_commit(a, "team genesis");
        repo
    }

    /// A governance change (a revoked recipient) signed only by `by`.
    pub fn revoke_recipient(&mut self, by: &MachineKey, recipient: &str) -> String {
        self.manifest.revoked_recipients.push(recipient.into());
        self.seal_commit(by, &format!("revoke {recipient}"))
    }

    /// A repository with config but no signed state (pre-ADR-0005).
    pub fn legacy(machine: &MachineKey) -> Self {
        let mut repo = Self::empty();
        repo.manifest
            .machines
            .insert(machine.name.clone(), machine.entry());
        repo.manifest.save(&repo.config).unwrap();
        repo.commit("legacy");
        repo
    }

    /// Genesis signed by `signer` although its governance lists only `listed`.
    pub fn genesis_signed_by_outsider(listed: &MachineKey, signer: &MachineKey) -> Self {
        let mut repo = Self::empty();
        repo.manifest
            .machines
            .insert(listed.name.clone(), listed.entry());
        repo.seal_commit(signer, "foreign genesis");
        repo
    }

    pub fn clone_from_bundle(bundle: &[u8]) -> Self {
        let repo = Self::empty();
        let file = repo.config.parent().unwrap().join("in.bundle");
        fs::write(&file, bundle).unwrap();
        git(
            &repo.config,
            &[
                "fetch",
                "-q",
                file.to_str().unwrap(),
                "refs/heads/main:refs/heads/incoming",
            ],
        );
        git(&repo.config, &["reset", "-q", "--hard", "incoming"]);
        let manifest = Manifest::load(&repo.config).unwrap();
        Self { manifest, ..repo }
    }

    pub fn commit(&self, message: &str) -> String {
        git(&self.config, &["add", "-A"]);
        git(
            &self.config,
            &["commit", "-q", "--allow-empty", "-m", message],
        );
        self.head()
    }

    /// Saves the manifest, signs the next epoch with `machine`, commits.
    pub fn seal_commit(&mut self, machine: &MachineKey, message: &str) -> String {
        self.manifest.save(&self.config).unwrap();
        seal(
            &self.config,
            &self.state,
            &self.manifest,
            &machine.signer,
            true,
        )
        .unwrap()
        .expect("sealed");
        self.commit(message)
    }

    pub fn add_machine(&mut self, by: &MachineKey, machine: &MachineKey) -> String {
        self.manifest
            .machines
            .insert(machine.name.clone(), machine.entry());
        self.seal_commit(by, &format!("add {}", machine.name))
    }

    /// An ordinary (unsigned-content) change: does not need a new epoch.
    pub fn touch(&self, file: &str, text: &str) -> String {
        fs::write(self.config.join(file), text).unwrap();
        self.commit(&format!("touch {file}"))
    }

    pub fn head(&self) -> String {
        git(&self.config, &["rev-parse", "HEAD"])
    }

    /// Bundle of `refs/heads/main`: everything, or `since..main`.
    pub fn bundle(&self, since: Option<&str>) -> Vec<u8> {
        let file = self
            .config
            .parent()
            .unwrap()
            .join(format!("{}.bundle", uuid::Uuid::new_v4()));
        let range = match since {
            Some(base) => format!("{base}..main"),
            None => "main".into(),
        };
        git(
            &self.config,
            &["bundle", "create", "-q", file.to_str().unwrap(), &range],
        );
        // `git bundle` names the ref `refs/heads/main` for both forms.
        let bytes = fs::read(&file).unwrap();
        fs::remove_file(&file).unwrap();
        bytes
    }
}

// ---------------------------------------------------------------------------
// Accounts, workspaces and links over HTTP
// ---------------------------------------------------------------------------

pub async fn account_token(s: &Stack, user: &UserSession) -> AccountToken {
    let client = http();
    let auth: DeviceAuthorization = client
        .post(s.url("/v1/device/authorize"))
        .json(&DeviceAuthorizeRequest {
            client_name: "cli".into(),
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
        .json(&DeviceApproveRequest {
            user_code: auth.user_code,
            decision: DeviceDecision::Approve,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    client
        .post(s.url("/v1/device/token"))
        .json(&DeviceTokenRequest {
            grant_type: DEVICE_CODE_GRANT_TYPE.into(),
            device_code: auth.device_code,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

pub async fn create_workspace(s: &Stack, user: &UserSession, name: &str) -> Workspace {
    let resp = http()
        .post(s.url("/v1/workspaces"))
        .header("Cookie", format!("__Host-mc_session={}", user.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &user.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&CreateWorkspace { name: name.into() })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    resp.json().await.unwrap()
}

/// A linked machine as the service knows it, able to sign requests.
pub struct Device<'a> {
    pub key: &'a MachineKey,
    pub workspace: String,
    pub id: String,
    pub token: String,
    pub linked: LinkedDevice,
}

pub async fn link<'a>(
    s: &Stack,
    token: &AccountToken,
    workspace: &str,
    key: &'a MachineKey,
) -> Device<'a> {
    let pk = key.public();
    let proof = key.signer.sign(&canonical_link_v1(
        workspace,
        &token.account_id,
        &key.name,
        &pk,
        &key.recipient,
    ));
    let resp = http()
        .post(s.url(&format!("/v1/workspaces/{workspace}/devices")))
        .header("Authorization", format!("Bearer {}", token.access_token))
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&LinkDeviceRequest {
            machine_name: key.name.clone(),
            signing_key: pk,
            recipient: key.recipient.clone(),
            proof,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let linked: LinkedDevice = resp.json().await.unwrap();
    Device {
        key,
        workspace: workspace.into(),
        id: linked.device_id.clone(),
        token: linked.device_token.clone(),
        linked,
    }
}

// ---------------------------------------------------------------------------
// Device-signed requests
// ---------------------------------------------------------------------------

/// Everything that goes into one signed request; tests tamper with fields.
#[derive(Clone)]
pub struct Signed {
    pub method: &'static str,
    /// Canonical path + query, also used as the request target.
    pub path_and_query: String,
    pub body: Vec<u8>,
    pub base: Option<String>,
    pub idempotency_key: Option<String>,
    pub timestamp: i64,
    pub nonce: String,
}

pub fn fresh_nonce() -> String {
    format!("n{}", uuid::Uuid::new_v4().simple())
}

pub fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

impl Device<'_> {
    pub fn sync_path(&self) -> String {
        format!("/v1/workspaces/{}/sync", self.workspace)
    }

    pub fn push(&self, bundle: Vec<u8>, base: Option<&str>) -> Signed {
        Signed {
            method: "POST",
            path_and_query: self.sync_path(),
            body: bundle,
            base: Some(base.unwrap_or(BASE_REVISION_NONE).to_string()),
            idempotency_key: Some(uuid::Uuid::new_v4().to_string()),
            timestamp: now(),
            nonce: fresh_nonce(),
        }
    }

    pub fn get(&self, path_and_query: &str) -> Signed {
        Signed {
            method: "GET",
            path_and_query: path_and_query.into(),
            body: Vec::new(),
            base: None,
            idempotency_key: None,
            timestamp: now(),
            nonce: fresh_nonce(),
        }
    }

    pub fn post_json(&self, path: &str, body: &impl serde::Serialize) -> Signed {
        Signed {
            method: "POST",
            path_and_query: path.into(),
            body: serde_json::to_vec(body).unwrap(),
            base: None,
            idempotency_key: None,
            timestamp: now(),
            nonce: fresh_nonce(),
        }
    }

    /// Signature over `req` with this device's key (protocol v1).
    pub fn signature(&self, req: &Signed) -> String {
        let sha = body_sha256_hex(&req.body);
        let ts = req.timestamp.to_string();
        let bytes = canonical_request_v1(&SignedRequestV1 {
            method: req.method,
            path_and_query: &req.path_and_query,
            workspace_id: &self.workspace,
            device_id: &self.id,
            body_sha256_hex: &sha,
            base_revision: req.base.as_deref().unwrap_or(""),
            idempotency_key: req.idempotency_key.as_deref().unwrap_or(""),
            timestamp: &ts,
            nonce: &req.nonce,
        });
        format!("v1={}", self.key.signer.sign(&bytes))
    }

    pub async fn send(&self, s: &Stack, req: &Signed) -> reqwest::Response {
        let signature = self.signature(req);
        self.send_with_signature(s, req, &signature).await
    }

    pub async fn send_with_signature(
        &self,
        s: &Stack,
        req: &Signed,
        signature: &str,
    ) -> reqwest::Response {
        let client = http();
        let url = s.url(&req.path_and_query);
        let mut builder = match req.method {
            "GET" => client.get(url),
            _ => client.post(url).body(req.body.clone()),
        };
        builder = builder
            .header("Authorization", format!("Bearer {}", self.token))
            .header(HEADER_DEVICE_ID, &self.id)
            .header(HEADER_TIMESTAMP, req.timestamp.to_string())
            .header(HEADER_NONCE, &req.nonce)
            .header(HEADER_CONTENT_SHA256, body_sha256_hex(&req.body))
            .header(HEADER_SIGNATURE, signature);
        if let Some(base) = &req.base {
            builder = builder.header(HEADER_BASE_REVISION, base);
        }
        if let Some(key) = &req.idempotency_key {
            builder = builder.header(HEADER_IDEMPOTENCY_KEY, key);
        }
        builder.send().await.unwrap()
    }
}
