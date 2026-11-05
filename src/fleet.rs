//! Machine enrollment requests: files under `<config_dir>/enroll/<name>.toml`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::MusterError;
use crate::manifest::{Manifest, MemberStatus, Role};
use crate::signing::{self, Signer};

const ENROLL_DIR: &str = "enroll";

/// Machine enrollment request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollmentRequest {
    pub name: String,
    pub recipient: String,
    /// Ed25519 public key (`musterpk1...`) of the requesting machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
    /// Random per-request nonce; bound into signed approvals (see `fleet_state`).
    #[serde(default)]
    pub nonce: String,
    pub platform: String,
    pub muster_version: String,
    #[serde(default)]
    pub agents: Vec<String>,
    pub created_at: String,
    /// Admin approvals so far, while the request waits for the team quorum
    /// (`[team] required_approvals`). Each is an Ed25519 signature bound to
    /// the request's content; see [`EnrollmentRequest::approval_message`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<ApprovalEntry>,
    /// Assignment proposed by the first approver; later approvals confirm it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
}

/// One admin's signed approval of an enrollment request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedApproval {
    pub member: String,
    /// The approving admin's machine (whose signing key signed).
    pub machine: String,
    pub signing_key: String,
    pub signature: String,
}

/// A recorded approval. `Legacy` is a bare member name from before signed
/// approvals; it is kept readable so it can be reported, but never counts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ApprovalEntry {
    Signed(SignedApproval),
    Legacy(String),
}

/// Approvals that count toward the quorum, and those that were ignored.
#[derive(Debug, Default)]
pub struct ApprovalCheck {
    /// Distinct active admins with a verifying signature, sorted.
    pub valid: Vec<SignedApproval>,
    /// Bare-name approvals from before signed approvals.
    pub legacy: usize,
    /// Signed entries that failed verification (altered request, unknown or
    /// non-admin signer, wrong key, bad signature, duplicate).
    pub invalid: usize,
}

const APPROVAL_DOMAIN: &str = "muster enroll approval v1";

impl EnrollmentRequest {
    /// Canonical bytes an admin signs: every field that decides what the
    /// approval grants, plus the random request nonce. Changing any of them
    /// (for example swapping the recipient) invalidates earlier approvals.
    pub fn approval_message(&self) -> Result<Vec<u8>, MusterError> {
        crate::fleet_state::canonical_bytes(&serde_json::json!({
            "domain": APPROVAL_DOMAIN,
            "name": self.name,
            "recipient": self.recipient,
            "signing_key": self.signing_key,
            "member": self.member,
            "role": self.role,
            "profile": self.profile,
            "created_at": self.created_at,
            "nonce": self.nonce,
        }))
    }

    /// Signs the request as `member`'s `machine` and records it.
    pub fn sign_approval(
        &mut self,
        member: &str,
        machine: &str,
        signer: &Signer,
    ) -> Result<(), MusterError> {
        let signature = signer.sign(&self.approval_message()?);
        self.approvals.push(ApprovalEntry::Signed(SignedApproval {
            member: member.to_string(),
            machine: machine.to_string(),
            signing_key: signer.public(),
            signature,
        }));
        Ok(())
    }

    /// Names of the members whose signed approvals are recorded (unverified;
    /// use [`Self::check_approvals`] for what counts).
    pub fn approval_members(&self) -> Vec<String> {
        self.approvals
            .iter()
            .filter_map(|a| match a {
                ApprovalEntry::Signed(s) => Some(s.member.clone()),
                ApprovalEntry::Legacy(_) => None,
            })
            .collect()
    }

    /// Verifies recorded approvals against the (already verified) governance in
    /// `manifest`: the signer must be a machine of an active admin that
    /// publishes exactly that signing key, and each admin counts once.
    pub fn check_approvals(&self, manifest: &Manifest) -> ApprovalCheck {
        let mut check = ApprovalCheck::default();
        let Ok(message) = self.approval_message() else {
            check.invalid = self.approvals.len();
            return check;
        };
        for entry in &self.approvals {
            let ApprovalEntry::Signed(approval) = entry else {
                check.legacy += 1;
                continue;
            };
            let admin = manifest.members.get(&approval.member).is_some_and(|m| {
                m.role == Role::Admin
                    && m.status == MemberStatus::Active
                    && m.machines.contains(&approval.machine)
            });
            let key_matches = manifest
                .machines
                .get(&approval.machine)
                .and_then(|m| m.signing_key.as_deref())
                == Some(approval.signing_key.as_str());
            let fresh = !check.valid.iter().any(|a| a.member == approval.member);
            if admin
                && key_matches
                && fresh
                && signing::verify(&approval.signing_key, &message, &approval.signature).is_ok()
            {
                check.valid.push(approval.clone());
            } else {
                check.invalid += 1;
            }
        }
        check.valid.sort_by(|a, b| a.member.cmp(&b.member));
        check
    }

    pub fn file_path(config_dir: &Path, name: &str) -> PathBuf {
        config_dir.join(ENROLL_DIR).join(format!("{name}.toml"))
    }

    pub fn save(&self, config_dir: &Path) -> Result<PathBuf, MusterError> {
        let dir = config_dir.join(ENROLL_DIR);
        fs::create_dir_all(&dir)
            .map_err(|e| MusterError::Operational(format!("{}: {e}", dir.display())))?;
        let path = Self::file_path(config_dir, &self.name);
        let text = toml::to_string_pretty(self)
            .map_err(|e| MusterError::Operational(format!("cannot serialize enrollment: {e}")))?;
        fs::write(&path, text)
            .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))?;
        Ok(path)
    }

    pub fn load(config_dir: &Path, name: &str) -> Result<Self, MusterError> {
        let path = Self::file_path(config_dir, name);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(MusterError::Operational(format!(
                    "no enrollment request found for '{name}' at {}",
                    path.display()
                )))
            }
            Err(e) => return Err(MusterError::Operational(format!("{}: {e}", path.display()))),
        };
        toml::from_str(&text).map_err(|e| MusterError::InvalidConfig {
            message: format!("corrupt enrollment request: {e}"),
            path: Some(path.display().to_string()),
        })
    }

    pub fn remove(config_dir: &Path, name: &str) -> Result<(), MusterError> {
        let path = Self::file_path(config_dir, name);
        if path.exists() {
            fs::remove_file(&path)
                .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))?;
        }
        Ok(())
    }

    /// Every pending enrollment request in `<config_dir>/enroll/*.toml`,
    /// ordered by name. A missing directory means there are none.
    pub fn pending(config_dir: &Path) -> Result<Vec<Self>, MusterError> {
        let dir = config_dir.join(ENROLL_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(MusterError::Operational(format!("{}: {e}", dir.display()))),
        };
        let mut requests = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|e| MusterError::Operational(format!("{}: {e}", dir.display())))?
                .path();
            if path.extension().is_some_and(|ext| ext == "toml") {
                let name = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or_default()
                    .to_owned();
                requests.push(Self::load(config_dir, &name)?);
            }
        }
        requests.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(requests)
    }
}
