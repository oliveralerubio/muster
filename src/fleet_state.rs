//! Signed fleet state: the repository is trusted only through a signature
//! checked against a local pin that lives outside the repo.
//!
//! - `<config_dir>/fleet.state.json`: `{payload, signature}`. The payload
//!   carries `epoch`, `prev` (hash of the previous payload), a digest of the
//!   governance fields of `muster.toml`, the SHA-256 of every vault `.age`
//!   file and the signer's public key.
//! - `<state_dir>/trust.json` (0600): the last accepted epoch, payload hash,
//!   governance digest and signer sets. Never in the repo.
//!
//! See `docs/adr/0005-signed-fleet-state.md`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::errors::MusterError;
use crate::manifest::{Manifest, MemberStatus, Role};
use crate::signing::{self, Signer};
use crate::vault::write_atomic;

pub const STATE_FILE: &str = "fleet.state.json";
pub const TRUST_FILE: &str = "trust.json";
const STATE_DOMAIN: &[u8] = b"muster fleet state v1\n";
const COSIGN_DOMAIN: &[u8] = b"muster fleet cosign v1\n";
const REJECT_DOMAIN: &[u8] = b"muster fleet reject v1\n";

/// `fleet.state.json`: the payload and its detached signature (hex), plus the
/// co-signatures of other admins over the same payload. Co-signatures are not
/// part of the signed payload (so adding one never changes the payload hash).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetState {
    pub payload: StatePayload,
    pub signature: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cosignatures: Vec<Cosignature>,
    /// Admins' signatures rejecting this payload (`muster fleet reject v1`):
    /// like co-signatures they sit outside the signed payload. Enough of them
    /// discard a pending proposal (`muster trust cosign --reject`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejections: Vec<Cosignature>,
}

/// One admin's signature over a state payload (`muster fleet cosign v1`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cosignature {
    pub signer: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatePayload {
    pub version: u32,
    pub epoch: u64,
    /// SHA-256 of the previous payload, `None` at epoch 1.
    pub prev: Option<String>,
    /// SHA-256 of the canonical governance content of `muster.toml`.
    pub governance: String,
    /// `group -> sha256(<group>.age)`.
    pub vault: BTreeMap<String, String>,
    /// `group -> sha256(<group>.meta.json)`: the plaintext sidecars (recipient
    /// and secret-name lists) are signed too. Omitted from the canonical bytes
    /// when empty, so states signed before sidecars were covered still verify
    /// (and are re-signed with digests by the next change).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vault_meta: BTreeMap<String, String>,
    /// Signed admin approvals behind machines this transition added while the
    /// team quorum (`[team] required_approvals`) is above one. Omitted from
    /// the canonical bytes when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enrollments: Vec<EnrollmentEvidence>,
    pub signer: String,
}

/// An enrollment request with the signed admin approvals that admitted it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollmentEvidence {
    pub name: String,
    pub recipient: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub created_at: String,
    pub nonce: String,
    pub approvals: Vec<crate::fleet::SignedApproval>,
}

/// Local trust pin (`<state_dir>/trust.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustAnchor {
    pub epoch: u64,
    pub payload_hash: String,
    pub governance: String,
    /// Keys that may sign governance changes (admins in team mode; every
    /// enrolled machine in single-person mode; always the recovery key).
    pub admins: Vec<String>,
    /// Keys that may re-sign when only vault ciphertext changed (any active
    /// member's machine, plus `admins`).
    pub writers: Vec<String>,
    /// The pin was created by trust on first use without `--expect-signer`:
    /// nobody confirmed the signer's fingerprint out of band yet (`doctor`
    /// flags it). Pins from before this field, and pins this machine signed
    /// itself, are confirmed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tofu: bool,
    /// Vault hashes of the accepted state: the baseline that tells which
    /// groups a later vault-only change touched. Empty on pins from before.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vault: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vault_meta: BTreeMap<String, String>,
    /// Canonical governance content of the accepted state, to diff against the
    /// next one (who was added, and by whom). `None` on pins from before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Value>,
}

impl TrustAnchor {
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(TRUST_FILE)
    }

    pub fn load(state_dir: &Path) -> Result<Option<Self>, MusterError> {
        let path = Self::path(state_dir);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(MusterError::Operational(format!("{}: {e}", path.display()))),
        };
        serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            MusterError::Security(format!("corrupt trust pin {}: {e}", path.display()))
        })
    }

    pub fn save(&self, state_dir: &Path) -> Result<(), MusterError> {
        fs::create_dir_all(state_dir)
            .map_err(|e| MusterError::Operational(format!("{}: {e}", state_dir.display())))?;
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| MusterError::Operational(format!("cannot serialize trust pin: {e}")))?;
        write_atomic(&Self::path(state_dir), &bytes)
    }

    pub(crate) fn for_state(
        state: &FleetState,
        manifest: &Manifest,
        tofu: bool,
    ) -> Result<Self, MusterError> {
        let (admins, writers) = signer_sets(manifest);
        Ok(Self {
            epoch: state.payload.epoch,
            payload_hash: state.payload_hash()?,
            governance: state.payload.governance.clone(),
            admins,
            writers,
            tofu,
            vault: state.payload.vault.clone(),
            vault_meta: state.payload.vault_meta.clone(),
            snapshot: Some(governance_content(manifest)),
        })
    }

    fn as_prior(&self) -> Prior {
        Prior {
            epoch: self.epoch,
            hash: self.payload_hash.clone(),
            governance: self.governance.clone(),
            admins: self.admins.clone(),
            writers: self.writers.clone(),
            vault: self.vault.clone(),
            vault_meta: self.vault_meta.clone(),
            snapshot: self.snapshot.clone(),
        }
    }
}

/// What a transition is checked against: the last accepted state, from the
/// local pin or from an intermediate commit of the history walk.
struct Prior {
    epoch: u64,
    hash: String,
    governance: String,
    admins: Vec<String>,
    writers: Vec<String>,
    vault: BTreeMap<String, String>,
    vault_meta: BTreeMap<String, String>,
    snapshot: Option<Value>,
}

impl Prior {
    fn from_state(state: &FleetState, manifest: &Manifest) -> Result<Self, MusterError> {
        let (admins, writers) = signer_sets(manifest);
        Ok(Self {
            epoch: state.payload.epoch,
            hash: state.payload_hash()?,
            governance: state.payload.governance.clone(),
            admins,
            writers,
            vault: state.payload.vault.clone(),
            vault_meta: state.payload.vault_meta.clone(),
            snapshot: Some(governance_content(manifest)),
        })
    }
}

// ---------------------------------------------------------------------------
// Canonical encoding
// ---------------------------------------------------------------------------

/// Canonical bytes of a JSON value: UTF-8, no insignificant whitespace, object
/// keys sorted bytewise, integers in decimal, strings escaped by `serde_json`.
/// Floating point numbers are refused (no canonical form).
pub fn canonical_bytes(value: &Value) -> Result<Vec<u8>, MusterError> {
    let mut out = Vec::new();
    write_canonical(value, &mut out)?;
    Ok(out)
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) -> Result<(), MusterError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => {
            if n.is_f64() {
                return Err(MusterError::Operational(
                    "floating point numbers have no canonical form".into(),
                ));
            }
            out.extend_from_slice(n.to_string().as_bytes());
        }
        Value::String(s) => out.extend_from_slice(
            serde_json::to_string(s)
                .map_err(|e| MusterError::Operational(format!("canonical string: {e}")))?
                .as_bytes(),
        ),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push(b'{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(&Value::String(key.clone()), out)?;
                out.push(b':');
                write_canonical(&map[key], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    signing::to_hex(&Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// Governance and vault digests
// ---------------------------------------------------------------------------

fn sorted(items: &[String]) -> Vec<String> {
    let mut v = items.to_vec();
    v.sort();
    v.dedup();
    v
}

/// The security-relevant content of `muster.toml`, as a canonical JSON value:
/// machines (recipient, signing key), members, roles, statuses and recorded
/// approvals, `[team]`, group readers, recovery, revoked recipients and
/// per-secret agent policies. Everything else (profiles, MCP, agents, last
/// sync times...) is deliberately outside the signature.
pub fn governance_content(manifest: &Manifest) -> Value {
    let machines: BTreeMap<_, _> = manifest
        .machines
        .iter()
        .map(|(n, m)| {
            (
                n.clone(),
                json!({"recipient": m.recipient, "signing_key": m.signing_key}),
            )
        })
        .collect();
    let members: BTreeMap<_, _> = manifest
        .members
        .iter()
        .map(|(n, m)| {
            (
                n.clone(),
                json!({
                    "role": m.role,
                    "status": m.status,
                    "machines": sorted(&m.machines),
                    "invited_by": m.invited_by,
                    "invite_nonce": m.invite_nonce,
                    "invited_at_state": m.invited_at_state,
                    "approved_by": m.approved_by,
                    "approvals": sorted(&m.approvals),
                }),
            )
        })
        .collect();
    let groups: BTreeMap<_, _> = manifest
        .groups
        .iter()
        .map(|(n, g)| {
            (
                n.clone(),
                json!({
                    "members": sorted(&g.readers.members),
                    "groups": sorted(&g.readers.groups),
                    "machines": sorted(&g.readers.machines),
                    "recipients": sorted(&g.readers.recipients),
                }),
            )
        })
        .collect();
    let secrets: BTreeMap<_, _> = manifest
        .secrets
        .iter()
        .map(|(r, p)| {
            let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
            if let Some(allow) = v.get_mut("allow") {
                *allow = json!(sorted(p.allow_list()));
            }
            (r.clone(), v)
        })
        .collect();
    let provider_json = |kind: Option<Value>,
                         base_url: &Option<String>,
                         key: &Option<String>,
                         api_key_helper: &Option<bool>| {
        json!({
            "kind": kind,
            "base_url": base_url,
            "key": key,
            "api_key_helper": api_key_helper,
        })
    };
    let providers: BTreeMap<_, _> = manifest
        .ai_providers
        .iter()
        .map(|(n, p)| {
            let kind = serde_json::to_value(p.kind).ok();
            (
                n.clone(),
                provider_json(kind, &p.base_url, &p.key, &p.api_key_helper),
            )
        })
        .collect();
    // Profile overrides can redirect a provider's endpoint or key just the same.
    let profile_providers: BTreeMap<_, _> = manifest
        .profiles
        .iter()
        .filter(|(_, p)| !p.ai_providers.is_empty())
        .map(|(name, profile)| {
            let overrides: BTreeMap<_, _> = profile
                .ai_providers
                .iter()
                .map(|(n, p)| {
                    let kind = p.kind.and_then(|k| serde_json::to_value(k).ok());
                    (
                        n.clone(),
                        provider_json(kind, &p.base_url, &p.key, &p.api_key_helper),
                    )
                })
                .collect();
            (name.clone(), overrides)
        })
        .collect();
    let mut content = json!({
        "machines": machines,
        "members": members,
        "groups": groups,
        "secrets": secrets,
        "providers": providers,
        "profile_providers": profile_providers,
        "mcp_reveal": sorted(
            manifest.mcp_server.as_ref().map(|m| m.reveal.as_slice()).unwrap_or_default()
        ),
        "required_approvals": manifest.required_approvals(),
        "revoked_recipients": sorted(&manifest.revoked_recipients),
        "recovery": manifest.recovery.as_ref().map(|r| json!({
            "recipient": r.recipient,
            "signing_key": r.signing_key,
        })),
    });
    // Omitted when empty so governance digests signed before revoked keys
    // were recorded stay the same.
    if !manifest.revoked_signing_keys.is_empty() {
        content["revoked_signing_keys"] = json!(sorted(&manifest.revoked_signing_keys));
    }
    content
}

/// Payload hash of the signed state in `config_dir` (`None` when unsigned):
/// what an enrollment or member approval is given against.
pub fn current_hash(config_dir: &Path) -> Result<Option<String>, MusterError> {
    FleetState::load(config_dir)?
        .map(|s| s.payload_hash())
        .transpose()
}

pub fn governance_digest(manifest: &Manifest) -> Result<String, MusterError> {
    Ok(sha256_hex(&canonical_bytes(&governance_content(manifest))?))
}

/// `group -> sha256` of every `vault/*.age` file.
pub fn vault_hashes(config_dir: &Path) -> Result<BTreeMap<String, String>, MusterError> {
    hash_vault_files(config_dir, |name| name.strip_suffix(".age"))
}

/// `group -> sha256` of every `vault/*.meta.json` sidecar.
pub fn vault_meta_hashes(config_dir: &Path) -> Result<BTreeMap<String, String>, MusterError> {
    hash_vault_files(config_dir, |name| name.strip_suffix(".meta.json"))
}

/// Hashes the vault files whose name `group_of` maps to a group.
fn hash_vault_files(
    config_dir: &Path,
    group_of: impl Fn(&str) -> Option<&str>,
) -> Result<BTreeMap<String, String>, MusterError> {
    let dir = config_dir.join("vault");
    let mut map = BTreeMap::new();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(map),
        Err(e) => return Err(MusterError::Operational(format!("{}: {e}", dir.display()))),
    };
    for entry in entries {
        let path = entry
            .map_err(|e| MusterError::Operational(format!("{}: {e}", dir.display())))?
            .path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if let Some(group) = group_of(name).map(str::to_string) {
            let bytes = fs::read(&path)
                .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))?;
            map.insert(group, sha256_hex(&bytes));
        }
    }
    Ok(map)
}

/// `(admins, writers)`: the signing keys allowed to change governance and
/// the ones allowed to re-sign after a vault-only change.
pub fn signer_sets(manifest: &Manifest) -> (Vec<String>, Vec<String>) {
    let key = |machine: &str| {
        manifest
            .machines
            .get(machine)
            .and_then(|m| m.signing_key.clone())
    };
    let mut admins = Vec::new();
    let mut writers = Vec::new();
    if manifest.members.is_empty() {
        admins.extend(manifest.machines.keys().filter_map(|m| key(m)));
        writers = admins.clone();
    } else {
        for member in manifest
            .members
            .values()
            .filter(|m| m.status == MemberStatus::Active)
        {
            let keys: Vec<String> = member.machines.iter().filter_map(|m| key(m)).collect();
            if member.role == Role::Admin {
                admins.extend(keys.clone());
            }
            writers.extend(keys);
        }
    }
    if let Some(recovery) = manifest
        .recovery
        .as_ref()
        .and_then(|r| r.signing_key.clone())
    {
        admins.push(recovery);
    }
    writers.extend(admins.iter().cloned());
    admins.sort();
    admins.dedup();
    writers.sort();
    writers.dedup();
    (admins, writers)
}

// ---------------------------------------------------------------------------
// The signed state
// ---------------------------------------------------------------------------

impl FleetState {
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(STATE_FILE)
    }

    pub fn load(config_dir: &Path) -> Result<Option<Self>, MusterError> {
        let path = Self::path(config_dir);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(MusterError::Operational(format!("{}: {e}", path.display()))),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| MusterError::Security(format!("unreadable {}: {e}", path.display())))
    }

    pub fn save(&self, config_dir: &Path) -> Result<(), MusterError> {
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| MusterError::Operational(format!("cannot serialize fleet state: {e}")))?;
        bytes.push(b'\n');
        write_atomic(&Self::path(config_dir), &bytes)
    }

    fn payload_bytes(&self) -> Result<Vec<u8>, MusterError> {
        let value = serde_json::to_value(&self.payload)
            .map_err(|e| MusterError::Operational(format!("cannot serialize payload: {e}")))?;
        canonical_bytes(&value)
    }

    pub fn payload_hash(&self) -> Result<String, MusterError> {
        Ok(sha256_hex(&self.payload_bytes()?))
    }

    fn signed_message(&self) -> Result<Vec<u8>, MusterError> {
        let mut msg = STATE_DOMAIN.to_vec();
        msg.extend(self.payload_bytes()?);
        Ok(msg)
    }

    pub fn verify_signature(&self) -> Result<(), MusterError> {
        signing::verify(
            &self.payload.signer,
            &self.signed_message()?,
            &self.signature,
        )
        .map_err(|_| MusterError::Security("fleet state signature is invalid".into()))
    }

    fn cosign_message(&self) -> Result<Vec<u8>, MusterError> {
        let mut msg = COSIGN_DOMAIN.to_vec();
        msg.extend(self.payload_bytes()?);
        Ok(msg)
    }

    /// Adds `signer`'s co-signature over this payload (once per key).
    pub fn add_cosignature(&mut self, signer: &Signer) -> Result<(), MusterError> {
        let key = signer.public();
        if self.valid_cosigners().contains(&key) {
            return Ok(());
        }
        // An entry that names this key but does not verify is a placeholder:
        // it is replaced, never allowed to shadow the genuine signature.
        self.cosignatures.retain(|c| c.signer != key);
        let signature = signer.sign(&self.cosign_message()?);
        self.cosignatures.push(Cosignature {
            signer: key,
            signature,
        });
        Ok(())
    }

    /// Drops every co-signature that does not verify over this payload (and
    /// repeats), so merging by signer can never let junk shadow a valid entry.
    fn retain_valid_cosignatures(&mut self) {
        let Ok(message) = self.cosign_message() else {
            self.cosignatures.clear();
            return;
        };
        let mut seen: Vec<String> = Vec::new();
        self.cosignatures.retain(|c| {
            let ok = !seen.contains(&c.signer)
                && signing::verify(&c.signer, &message, &c.signature).is_ok();
            if ok {
                seen.push(c.signer.clone());
            }
            ok
        });
    }

    /// Keys whose co-signature verifies over this payload, once each. The
    /// proposer's own seal is not listed here.
    pub fn valid_cosigners(&self) -> Vec<String> {
        let Ok(message) = self.cosign_message() else {
            return Vec::new();
        };
        let mut valid: Vec<String> = Vec::new();
        for c in &self.cosignatures {
            if !valid.contains(&c.signer)
                && signing::verify(&c.signer, &message, &c.signature).is_ok()
            {
                valid.push(c.signer.clone());
            }
        }
        valid
    }

    fn reject_message(&self) -> Result<Vec<u8>, MusterError> {
        let mut msg = REJECT_DOMAIN.to_vec();
        msg.extend(self.payload_bytes()?);
        Ok(msg)
    }

    /// Adds `signer`'s rejection of this payload (once per key; a placeholder
    /// naming the key is replaced).
    pub fn add_rejection(&mut self, signer: &Signer) -> Result<(), MusterError> {
        let key = signer.public();
        if self.valid_rejecters().contains(&key) {
            return Ok(());
        }
        self.rejections.retain(|c| c.signer != key);
        let signature = signer.sign(&self.reject_message()?);
        self.rejections.push(Cosignature {
            signer: key,
            signature,
        });
        Ok(())
    }

    /// Keys whose rejection verifies over this payload, once each.
    pub fn valid_rejecters(&self) -> Vec<String> {
        let Ok(message) = self.reject_message() else {
            return Vec::new();
        };
        let mut valid: Vec<String> = Vec::new();
        for c in &self.rejections {
            if !valid.contains(&c.signer)
                && signing::verify(&c.signer, &message, &c.signature).is_ok()
            {
                valid.push(c.signer.clone());
            }
        }
        valid
    }

    fn sign(payload: StatePayload, signer: &Signer) -> Result<Self, MusterError> {
        let mut state = Self {
            payload,
            signature: String::new(),
            cosignatures: Vec::new(),
            rejections: Vec::new(),
        };
        state.signature = signer.sign(&state.signed_message()?);
        Ok(state)
    }
}

/// Whether signing now would change the signed content.
pub fn is_current(
    config_dir: &Path,
    manifest: &Manifest,
    state: &FleetState,
) -> Result<bool, MusterError> {
    Ok(state.payload.governance == governance_digest(manifest)?
        && state.payload.vault == vault_hashes(config_dir)?
        && state.payload.vault_meta == vault_meta_hashes(config_dir)?)
}

/// Refuses a repo whose signed state is older than the local pin. `action`
/// completes "cannot ... a rolled-back fleet state" in the message.
///
/// A missing state is not checked here (the downgrade check in
/// [`verify_with`] owns that); a state without a pin is fine.
pub fn ensure_not_behind_pin(
    config_dir: &Path,
    state_dir: &Path,
    action: &str,
) -> Result<(), MusterError> {
    let (Some(pin), Some(state)) = (TrustAnchor::load(state_dir)?, FleetState::load(config_dir)?)
    else {
        return Ok(());
    };
    if state.payload.epoch < pin.epoch {
        return Err(MusterError::Security(format!(
            "cannot {action} a rolled-back fleet state (epoch {} < pinned {}); restore the repo              from a trusted copy",
            state.payload.epoch, pin.epoch
        )));
    }
    Ok(())
}

/// Signs the current repository content as the next epoch (epoch 1 when there
/// is no state yet) and advances the local pin. Returns `None` when nothing
/// signed changed and `force` is false.
pub fn seal(
    config_dir: &Path,
    state_dir: &Path,
    manifest: &Manifest,
    signer: &Signer,
    force: bool,
) -> Result<Option<FleetState>, MusterError> {
    seal_with(config_dir, state_dir, manifest, signer, force, Vec::new())
}

/// [`seal`] carrying the signed enrollment approvals behind a quorum.
pub fn seal_with(
    config_dir: &Path,
    state_dir: &Path,
    manifest: &Manifest,
    signer: &Signer,
    force: bool,
    enrollments: Vec<EnrollmentEvidence>,
) -> Result<Option<FleetState>, MusterError> {
    seal_inner(
        config_dir,
        state_dir,
        manifest,
        signer,
        force,
        enrollments,
        true,
    )
}

/// Like [`seal_with`] but leaves the local pin alone: the caller publishes the
/// state later (see `revocation_tx`) and advances the pin only then.
pub fn seal_unpinned(
    config_dir: &Path,
    state_dir: &Path,
    manifest: &Manifest,
    signer: &Signer,
    enrollments: Vec<EnrollmentEvidence>,
) -> Result<Option<FleetState>, MusterError> {
    seal_inner(
        config_dir,
        state_dir,
        manifest,
        signer,
        false,
        enrollments,
        false,
    )
}

fn seal_inner(
    config_dir: &Path,
    state_dir: &Path,
    manifest: &Manifest,
    signer: &Signer,
    force: bool,
    enrollments: Vec<EnrollmentEvidence>,
    save_pin: bool,
) -> Result<Option<FleetState>, MusterError> {
    // A seal must advance or preserve the pinned epoch, never launder an older state.
    ensure_not_behind_pin(config_dir, state_dir, "re-sign")?;
    let current = FleetState::load(config_dir)?;
    if let (Some(pin), Some(state)) = (TrustAnchor::load(state_dir)?, &current) {
        if state.payload.epoch > pin.epoch {
            return Err(MusterError::Security(format!(
                "cannot sign on top of fleet state epoch {} which this machine has not \
                 accepted (pinned epoch {}); a governance change may be waiting for admin \
                 co-signatures (`muster trust cosign`)",
                state.payload.epoch, pin.epoch
            )));
        }
    }
    if let Some(state) = &current {
        if !force && is_current(config_dir, manifest, state)? {
            return Ok(None);
        }
    }
    let (epoch, prev) = match &current {
        Some(state) => (state.payload.epoch + 1, Some(state.payload_hash()?)),
        None => (1, None),
    };
    let payload = StatePayload {
        version: 1,
        epoch,
        prev,
        governance: governance_digest(manifest)?,
        vault: vault_hashes(config_dir)?,
        vault_meta: vault_meta_hashes(config_dir)?,
        enrollments,
        signer: signer.public(),
    };
    let state = FleetState::sign(payload, signer)?;
    state.save(config_dir)?;
    // The local pin only moves to a state the exact rule peers apply accepts:
    // a change short of its admin quorum is proposed, not accepted, and one
    // peers would refuse outright (the push gate then refuses it too) never
    // becomes this machine's own baseline.
    if save_pin {
        let accepted = match TrustAnchor::load(state_dir)? {
            Some(pin) => {
                state.payload.epoch == pin.epoch + 1
                    && matches!(
                        check_transition(&pin.as_prior(), &state, manifest),
                        Ok(Transition::Accepted)
                    )
            }
            None => true,
        };
        if accepted {
            TrustAnchor::for_state(&state, manifest, false)?.save(state_dir)?;
        }
    }
    Ok(Some(state))
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Transitions: the chain from the pin to the current state
// ---------------------------------------------------------------------------

/// What walking the transitions from the pin to the current state found.
struct Chain {
    /// How the current state itself fared.
    outcome: Transition,
    /// The newest accepted intermediate state (strictly between the pin and
    /// the current state) with its manifest, `None` when the current state
    /// directly follows the pin. The pin advances to it while the current
    /// state is pending.
    accepted: Option<(FleetState, Manifest)>,
    /// The last accepted state the current state is judged against.
    prior: Prior,
}

/// Verifies every transition from the pin to `state`. The next epoch is
/// checked directly; a larger jump walks the git history of the state file so
/// that each intermediate state is itself signed by someone the one before it
/// authorized (a revoked admin key cannot skip ahead).
fn verify_chain(
    config_dir: &Path,
    pin: &TrustAnchor,
    state: &FleetState,
    manifest: &Manifest,
) -> Result<Chain, MusterError> {
    if state.payload.epoch == pin.epoch + 1 {
        return Ok(Chain {
            outcome: check_transition(&pin.as_prior(), state, manifest)?,
            accepted: None,
            prior: pin.as_prior(),
        });
    }
    let jump = |why: String| {
        MusterError::Security(format!(
            "epoch jump without valid intermediate chain: {why}"
        ))
    };
    if !crate::git::is_repo(config_dir) {
        return Err(jump(format!(
            "epochs {}..{} are not in a git history to check",
            pin.epoch + 1,
            state.payload.epoch
        )));
    }
    let by_epoch = history_chain(config_dir, pin.epoch + 1, state, manifest)?;
    let mut prior = pin.as_prior();
    let mut outcome = Transition::Accepted;
    let mut accepted: Option<(FleetState, Manifest)> = None;
    let mut accepted_prior = pin.as_prior();
    for epoch in pin.epoch + 1..=state.payload.epoch {
        let Some((step, step_manifest)) = by_epoch.get(&epoch) else {
            return Err(jump(format!(
                "epoch {epoch} is missing from the repository history"
            )));
        };
        step.verify_signature()?;
        if step.payload.governance != governance_digest(step_manifest)? {
            return Err(jump(format!(
                "epoch {epoch} does not match the muster.toml committed with it"
            )));
        }
        let manifest_for_step = if epoch == state.payload.epoch {
            if step.payload_hash()? != state.payload_hash()? {
                return Err(jump(
                    "the current state is not the end of the recorded history".into(),
                ));
            }
            manifest
        } else {
            step_manifest
        };
        // The current state carries the co-signatures seen so far.
        let to_check = if epoch == state.payload.epoch {
            state
        } else {
            step
        };
        outcome = check_transition(&prior, to_check, manifest_for_step).map_err(|e| match e {
            MusterError::Security(m) => jump(format!("epoch {epoch}: {m}")),
            other => other,
        })?;
        if epoch != state.payload.epoch && outcome != Transition::Accepted {
            return Err(jump(format!(
                "epoch {epoch} never gathered its required admin signatures"
            )));
        }
        prior = Prior::from_state(step, step_manifest)?;
        if epoch != state.payload.epoch {
            accepted = Some((step.clone(), step_manifest.clone()));
            accepted_prior = Prior::from_state(step, step_manifest)?;
        }
    }
    Ok(Chain {
        outcome,
        accepted,
        prior: accepted_prior,
    })
}

/// The chain of signed states ending at `head`, oldest epoch `min`, each with
/// the `muster.toml` committed alongside it. Epochs below `min` and gaps end
/// the chain (the caller reports the missing epoch).
///
/// The chain is followed through `prev` links starting from `head`, never by
/// commit order: for each epoch only the state whose payload hash `prev` names
/// is taken, so a junk or forked state committed earlier at the same epoch
/// cannot displace the real one. A candidate is usable only when its signature
/// verifies and the manifest recomputes the governance digest it signed, so no
/// admin or signer set is ever derived from an unauthenticated manifest.
/// Versions of one payload are merged: every co-signature that verifies is
/// kept (only co-signers the predecessor authorized ever count).
fn history_chain(
    config_dir: &Path,
    min: u64,
    head: &FleetState,
    head_manifest: &Manifest,
) -> Result<BTreeMap<u64, (FleetState, Manifest)>, MusterError> {
    let mut candidates: BTreeMap<String, (FleetState, Manifest)> = BTreeMap::new();
    for commit in crate::git::commits_touching_oldest_first(config_dir, STATE_FILE)? {
        let Some(text) = crate::git::file_at(config_dir, &commit, STATE_FILE) else {
            continue;
        };
        let Ok(mut past) = serde_json::from_str::<FleetState>(&text) else {
            continue;
        };
        let epoch = past.payload.epoch;
        if epoch < min || epoch >= head.payload.epoch || past.verify_signature().is_err() {
            continue;
        }
        let Ok(hash) = past.payload_hash() else {
            continue;
        };
        // Only co-signatures that verify are ever merged by signer.
        past.retain_valid_cosignatures();
        if let Some((seen, _)) = candidates.get_mut(&hash) {
            for c in past.cosignatures {
                if !seen.cosignatures.iter().any(|x| x.signer == c.signer) {
                    seen.cosignatures.push(c);
                }
            }
            continue;
        }
        let Some(manifest_text) =
            crate::git::file_at(config_dir, &commit, crate::manifest::FILE_NAME)
        else {
            continue;
        };
        let Ok(past_manifest) = Manifest::parse(&manifest_text, crate::manifest::FILE_NAME) else {
            continue;
        };
        if governance_digest(&past_manifest).ok().as_deref() != Some(&past.payload.governance) {
            continue;
        }
        candidates.insert(hash, (past, past_manifest));
    }
    let mut chain = BTreeMap::new();
    chain.insert(head.payload.epoch, (head.clone(), head_manifest.clone()));
    let mut want = head.payload.prev.clone();
    for epoch in (min..head.payload.epoch).rev() {
        let Some((prev, prev_manifest)) = want.as_ref().and_then(|h| candidates.get(h)) else {
            break;
        };
        if prev.payload.epoch != epoch {
            break;
        }
        want = prev.payload.prev.clone();
        chain.insert(epoch, (prev.clone(), prev_manifest.clone()));
    }
    Ok(chain)
}

/// A machine with no pin must not adopt authority from a history that was
/// never accepted. The history is evaluated as one forward sequence of
/// transitions from genesis (epoch 1, `prev` empty, signed by a key its own
/// governance authorizes): a state supplies authority only when its own
/// transition was accepted (quorate), so a pending intermediate state ends
/// the accepted prefix even if a later transition would pass in isolation.
/// Returns `None` when `state` itself is the end of an accepted chain;
/// otherwise the first non-accepted outcome with the newest accepted state
/// (the one to pin).
fn bootstrap_hold(
    config_dir: &Path,
    state: &FleetState,
    manifest: &Manifest,
) -> Result<Option<(Transition, FleetState, Manifest)>, MusterError> {
    let epoch = state.payload.epoch;
    let refuse = |why: String| {
        MusterError::Security(format!(
            "cannot pin this fleet: {why}; ask an admin to confirm the state out of band"
        ))
    };
    if epoch <= 1 {
        if state.payload.prev.is_some() {
            return Err(refuse("its first state names a predecessor".into()));
        }
        return Ok(None);
    }
    if !crate::git::is_repo(config_dir) {
        return Err(refuse(format!(
            "its state is at epoch {epoch} but there is no git history to check the admin signatures \
             of its changes"
        )));
    }
    let chain = history_chain(config_dir, 1, state, manifest)?;
    for e in 1..=epoch {
        if !chain.contains_key(&e) {
            return Err(refuse(format!(
                "epoch {e} is missing from the repository history (or does not chain to the \
                 current state)"
            )));
        }
    }
    let (genesis, genesis_manifest) = &chain[&1];
    if genesis.payload.prev.is_some() {
        return Err(refuse("its first state names a predecessor".into()));
    }
    let (admins, writers) = signer_sets(genesis_manifest);
    if !admins.contains(&genesis.payload.signer) && !writers.contains(&genesis.payload.signer) {
        return Err(refuse(
            "its first state is signed by a key its own governance does not authorize".into(),
        ));
    }
    let mut prior = Prior::from_state(genesis, genesis_manifest)?;
    let mut accepted = (genesis.clone(), genesis_manifest.clone());
    for e in 2..=epoch {
        let (step, step_manifest) = &chain[&e];
        let outcome = check_transition(&prior, step, step_manifest).map_err(|err| match err {
            MusterError::Security(m) if e < epoch => refuse(format!("epoch {e}: {m}")),
            other => other,
        })?;
        if outcome != Transition::Accepted {
            return Ok(Some((outcome, accepted.0, accepted.1)));
        }
        prior = Prior::from_state(step, step_manifest)?;
        accepted = (step.clone(), step_manifest.clone());
    }
    Ok(None)
}

/// One transition `prior` -> `state`: signer authorization (admin for
/// governance changes, a reader of every touched group for vault-only ones),
/// the predecessor link, and the signed quorum for added machines/members.
fn check_transition(
    prior: &Prior,
    state: &FleetState,
    manifest: &Manifest,
) -> Result<Transition, MusterError> {
    let signer = &state.payload.signer;
    let vault_only = state.payload.governance == prior.governance;
    let allowed = if vault_only {
        &prior.writers
    } else {
        &prior.admins
    };
    if !allowed.contains(signer) {
        return Err(MusterError::Security(format!(
            "fleet state signed by {} which is not authorized {} under the last \
             accepted state",
            signing::fingerprint(signer),
            if vault_only {
                "to update the vault"
            } else {
                "to change governance"
            }
        )));
    }
    if state.payload.epoch != prior.epoch + 1
        || state.payload.prev.as_deref() != Some(prior.hash.as_str())
    {
        return Err(MusterError::Security(
            "fleet state history forked: its predecessor is not the accepted state".into(),
        ));
    }
    // Sidecar digests, once signed, stay signed: only states from before
    // sidecars were covered may omit them, never a later one.
    if !prior.vault_meta.is_empty()
        && state.payload.vault_meta.is_empty()
        && !state.payload.vault.is_empty()
    {
        return Err(MusterError::Security(
            "fleet state drops the signed metadata sidecar digests the accepted state carried \
             (downgrade into the legacy window)"
                .into(),
        ));
    }
    // The vault rule holds for every transition, whatever else it changes: a
    // governance edit that is exempt from the quorum (an invitation, an
    // approved enrollment) never carries ciphertext rewrites along with it.
    let vault = vault_gate(prior, state, manifest)?;
    if !vault_only {
        if let Some(old) = &prior.snapshot {
            let new = governance_content(manifest);
            check_quorum(old, &new, state, &prior.hash)?;
            return Ok(match transition_outcome(prior, state, &new) {
                Transition::Accepted => vault.unwrap_or(Transition::Accepted),
                pending => pending,
            });
        }
    }
    Ok(vault.unwrap_or(Transition::Accepted))
}

/// The vault part of a transition. A signer that is not an admin must read
/// every group it changed (refused otherwise). An admin rewriting a group it
/// cannot read is not a routine write: with several admins required it needs
/// their co-signatures, like a governance change (`Some(Pending)`). `None`
/// when the vault part is fine.
fn vault_gate(
    prior: &Prior,
    state: &FleetState,
    manifest: &Manifest,
) -> Result<Option<Transition>, MusterError> {
    if prior.snapshot.is_none() {
        return Ok(None);
    }
    let signer = &state.payload.signer;
    let changed = changed_groups(
        &prior.vault,
        &prior.vault_meta,
        &state.payload.vault,
        &state.payload.vault_meta,
    );
    if changed.is_empty() {
        return Ok(None);
    }
    if !prior.admins.contains(signer) {
        ensure_reads_groups(manifest, signer, &changed)?;
        return Ok(None);
    }
    if ensure_reads_groups(manifest, signer, &changed).is_err() {
        if let Some((received, required)) = quorum_count(prior, state) {
            if received < required {
                return Ok(Some(Transition::Pending { received, required }));
            }
        }
    }
    Ok(None)
}

/// How a transition fared against the k-of-n rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Accepted,
    /// Signed by fewer than `required` distinct admins of the prior state.
    Pending {
        received: usize,
        required: usize,
    },
}

/// Who a signing key is for counting purposes: one vote per admin member (two
/// machines of one admin are still one admin), the recovery key on its own.
fn voter(snapshot: &Value, key: &str) -> String {
    if let Some(members) = snapshot["members"].as_object() {
        for (name, member) in members {
            if member["role"] != "admin" || member["status"] != "active" {
                continue;
            }
            let owns = member["machines"].as_array().is_some_and(|machines| {
                machines.iter().any(|m| {
                    m.as_str()
                        .is_some_and(|m| snapshot["machines"][m]["signing_key"] == key)
                })
            });
            if owns {
                return format!("member:{name}");
            }
        }
    }
    format!("key:{key}")
}

/// `(received, required)` when the prior state demands several admins for a
/// governance change (team mode, `required_approvals > 1`), `None` otherwise.
/// The recovery key alone always satisfies it (break-glass).
fn quorum_count(prior: &Prior, state: &FleetState) -> Option<(usize, usize)> {
    let old = prior.snapshot.as_ref()?;
    let required = old["required_approvals"].as_u64().unwrap_or(1) as usize;
    let team_mode = old["members"].as_object().is_some_and(|m| !m.is_empty());
    if required <= 1 || !team_mode {
        return None;
    }
    let mut keys = vec![state.payload.signer.clone()];
    keys.extend(state.valid_cosigners());
    if keys
        .iter()
        .any(|k| old["recovery"]["signing_key"].as_str() == Some(k.as_str()))
    {
        return Some((required, required));
    }
    let mut voters: Vec<String> = Vec::new();
    for key in keys.iter().filter(|k| prior.admins.contains(k)) {
        let v = voter(old, key);
        if !voters.contains(&v) {
            voters.push(v);
        }
    }
    Some((voters.len(), required))
}

/// A machine `new` adds (absent from `old`) whose age recipient is in the old
/// `revoked_recipients` or whose signing key is in the old
/// `revoked_signing_keys`.
fn revoked_machine_added(old: &Value, new: &Value) -> Option<String> {
    let listed = |field: &str, value: &Value| {
        value.is_string()
            && old[field]
                .as_array()
                .is_some_and(|list| list.iter().any(|x| x == value))
    };
    new["machines"].as_object()?.iter().find_map(|(name, m)| {
        let known = old["machines"].get(name).is_some_and(|o| o == m);
        (!known
            && (listed("revoked_recipients", &m["recipient"])
                || listed("revoked_signing_keys", &m["signing_key"])))
        .then(|| name.clone())
    })
}

/// A machine of `member` whose recipient or signing key the old state revoked.
fn revoked_member_machine(old: &Value, new: &Value, member: &Value) -> Option<String> {
    let listed = |field: &str, value: &Value| {
        value.is_string()
            && old[field]
                .as_array()
                .is_some_and(|list| list.iter().any(|x| x == value))
    };
    member["machines"].as_array()?.iter().find_map(|m| {
        let name = m.as_str()?;
        let machine = if new["machines"][name].is_object() {
            &new["machines"][name]
        } else {
            &old["machines"][name]
        };
        (listed("revoked_recipients", &machine["recipient"])
            || listed("revoked_signing_keys", &machine["signing_key"]))
        .then(|| name.to_string())
    })
}

/// Whether the diff only enrolls brand-new machines whose own signed
/// approvals (checked by [`check_quorum`]) already carry the quorum, and
/// activates members that carry theirs. A machine is brand new when its name,
/// age recipient and signing key appear nowhere in the prior governance. Who
/// owns a signing key is itself governance: any change to the machine list of
/// an existing machine's owner, a re-assignment, a re-keying, or a machine
/// listed under two members needs the full co-signature quorum. Every added
/// machine must join exactly the member its approved request named.
fn enrollment_only(old: &Value, new: &Value, state: &FleetState) -> bool {
    let (Some(o), Some(n)) = (old.as_object(), new.as_object()) else {
        return false;
    };
    if o.keys()
        .chain(n.keys())
        .any(|k| !matches!(k.as_str(), "machines" | "members") && o.get(k) != n.get(k))
    {
        return false;
    }
    let (Some(old_machines), Some(new_machines)) =
        (old["machines"].as_object(), new["machines"].as_object())
    else {
        return false;
    };
    if !old_machines
        .iter()
        .all(|(name, m)| new_machines.get(name) == Some(m))
    {
        return false;
    }
    // Everything the prior governance already knows: no key or recipient of a
    // new machine may be one of these.
    let known = |field: &str, value: &Value| {
        value.is_string()
            && (old_machines.values().any(|m| &m[field] == value)
                || old["recovery"].get(field) == Some(value))
    };
    if revoked_machine_added(old, new).is_some() {
        return false;
    }
    let added: Vec<&String> = new_machines
        .keys()
        .filter(|name| !old_machines.contains_key(*name))
        .collect();
    for (i, name) in added.iter().enumerate() {
        let m = &new_machines[*name];
        if known("recipient", &m["recipient"]) || known("signing_key", &m["signing_key"]) {
            return false;
        }
        // Two new machines may not share a recipient or key either.
        if added[..i].iter().any(|other| {
            new_machines[*other]["recipient"] == m["recipient"]
                || (m["signing_key"].is_string()
                    && new_machines[*other]["signing_key"] == m["signing_key"])
        }) {
            return false;
        }
    }
    // Every machine of a member that is not already in the member's prior list
    // must be a new machine whose approved request named this member.
    let Some(new_members) = new["members"].as_object() else {
        return false;
    };
    let joins_rightly = |member: &str, machine: &Value| {
        let Some(machine) = machine.as_str() else {
            return false;
        };
        if new_members[member]["role"] == "admin" || old["members"][member]["role"] == "admin" {
            return false;
        }
        added.iter().any(|a| a.as_str() == machine)
            && state.payload.enrollments.iter().any(|e| {
                e.name == machine
                    && e.member.as_deref() == Some(member)
                    && Value::String(e.recipient.clone()) == new_machines[machine]["recipient"]
                    && e.signing_key.as_deref() == new_machines[machine]["signing_key"].as_str()
            })
    };
    let mut owned: Vec<&str> = Vec::new();
    for (name, nm) in new_members {
        let om = &old["members"][name];
        let old_list: Vec<&Value> = om["machines"]
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        for machine in nm["machines"].as_array().into_iter().flatten() {
            if let Some(m) = machine.as_str() {
                if owned.contains(&m) {
                    return false;
                }
                owned.push(m);
            }
            if !old_list.contains(&machine) && !joins_rightly(name, machine) {
                return false;
            }
        }
    }
    // In team mode, every newly added machine must belong to an approved owner.
    // An orphan machine is refused.
    if !new_members.is_empty() && !added.iter().all(|m| owned.contains(&m.as_str())) {
        return false;
    }
    let subset = |a: &Value, b: &Value| {
        let (a, b) = (a.as_array(), b.as_array());
        a.is_none_or(|a| b.is_some_and(|b| a.iter().all(|x| b.contains(x))))
    };
    old["members"].as_object().is_some_and(|ms| {
        ms.iter().all(|(name, om)| {
            let nm = &new["members"][name];
            // Dropping an invitation that never became active takes nothing away.
            let dropped_invitation = nm.is_null()
                && om["status"] != "active"
                && om["machines"].as_array().is_none_or(|m| m.is_empty());
            if dropped_invitation {
                return true;
            }
            let activated = om["status"] != "active" && nm["status"] == "active";
            nm.is_object()
                && om["role"] == nm["role"]
                && om["invited_by"] == nm["invited_by"]
                && om["invite_nonce"] == nm["invite_nonce"]
                && om["invited_at_state"] == nm["invited_at_state"]
                && (om["status"] == nm["status"] || activated)
                && (om["approved_by"] == nm["approved_by"] || activated)
                && subset(&om["machines"], &nm["machines"])
                && subset(&om["approvals"], &nm["approvals"])
        })
    })
}

fn transition_outcome(prior: &Prior, state: &FleetState, new: &Value) -> Transition {
    let (Some(old), Some((received, required))) = (&prior.snapshot, quorum_count(prior, state))
    else {
        return Transition::Accepted;
    };
    if received >= required || enrollment_only(old, new, state) {
        Transition::Accepted
    } else {
        Transition::Pending { received, required }
    }
}

/// Unified diff of the governance content, for the operator to review.
pub fn governance_diff(old: Option<&Value>, new: &Value) -> String {
    let render = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
    crate::engine::diff::unified(
        &old.map(render).unwrap_or_default(),
        &render(new),
        "governance",
        false,
    )
}

/// After sealing: why the rule peers apply refuses `state` outright (not
/// merely pending), `None` when they would take or hold it.
pub fn refused_by_peers(
    state_dir: &Path,
    state: &FleetState,
    manifest: &Manifest,
) -> Result<Option<String>, MusterError> {
    let Some(pin) = TrustAnchor::load(state_dir)? else {
        return Ok(None);
    };
    if state.payload.epoch != pin.epoch + 1 {
        return Ok(None);
    }
    Ok(match check_transition(&pin.as_prior(), state, manifest) {
        Err(MusterError::Security(why)) => Some(why),
        _ => None,
    })
}

/// After sealing: `(received, required)` when the new state lacks the admin
/// signatures the pin's team demands, i.e. peers will hold it as pending.
pub fn pending_quorum(
    state_dir: &Path,
    state: &FleetState,
    manifest: &Manifest,
) -> Result<Option<(usize, usize)>, MusterError> {
    let Some(pin) = TrustAnchor::load(state_dir)? else {
        return Ok(None);
    };
    if state.payload.epoch != pin.epoch + 1 {
        return Ok(None);
    }
    let prior = pin.as_prior();
    let outcome = if state.payload.governance == pin.governance {
        Transition::Accepted
    } else {
        transition_outcome(&prior, state, &governance_content(manifest))
    };
    // An admin rewriting groups it does not read is pending whatever else the
    // transition changes (see `vault_gate`).
    let outcome = match (outcome, vault_gate(&prior, state, manifest)) {
        (Transition::Accepted, Ok(Some(pending))) => pending,
        (outcome, _) => outcome,
    };
    Ok(match outcome {
        Transition::Pending { received, required } => Some((received, required)),
        Transition::Accepted => None,
    })
}

/// A governance change this machine has not accepted, with the state it
/// would replace.
pub struct PendingProposal {
    /// The current (unaccepted) state, with the signatures it carries.
    pub head: FleetState,
    /// Newest accepted state, recomputed from the history (never the possibly
    /// stale local pin): payload hash and epoch.
    pub accepted_hash: String,
    pub accepted_epoch: u64,
    prior: Prior,
}

impl PendingProposal {
    /// Whether `key` may take part in deciding the fate of the proposal: an
    /// admin of the accepted state.
    pub fn is_admin(&self, key: &str) -> bool {
        self.prior.admins.contains(&key.to_string())
    }

    /// `(received, required)` reject votes: distinct admins of the accepted
    /// state whose rejection signature verifies, so one compromised admin
    /// never decides alone. The recovery key alone is enough (break-glass).
    pub fn reject_tally(&self, state: &FleetState) -> (usize, usize) {
        let Some(old) = self.prior.snapshot.as_ref() else {
            return (1, 1);
        };
        let team_mode = old["members"].as_object().is_some_and(|m| !m.is_empty());
        let required = if team_mode {
            old["required_approvals"].as_u64().unwrap_or(1).max(1) as usize
        } else {
            1
        };
        let recovery = old["recovery"]["signing_key"].as_str();
        let mut voters: Vec<String> = Vec::new();
        for key in state.valid_rejecters() {
            if recovery == Some(key.as_str()) {
                return (required, required);
            }
            if !self.prior.admins.contains(&key) {
                continue;
            }
            let v = voter(old, &key);
            if !voters.contains(&v) {
                voters.push(v);
            }
        }
        (voters.len(), required)
    }
}

/// The unaccepted proposal at the head of the repo, found by walking the
/// accepted transitions forward from the local pin. Errors with `Usage` when
/// the head is accepted (or already pinned) and with `Security` when the
/// history does not chain.
pub fn pending_proposal(
    config_dir: &Path,
    state_dir: &Path,
    manifest: &Manifest,
) -> Result<PendingProposal, MusterError> {
    let nothing = || {
        MusterError::Usage(
            "no pending proposal to reject; the repo state is already accepted".into(),
        )
    };
    let pin = TrustAnchor::load(state_dir)?.ok_or_else(|| {
        MusterError::Security("no local trust pin; cannot determine which state is accepted".into())
    })?;
    let head = FleetState::load(config_dir)?
        .ok_or_else(|| MusterError::Security("the repo has no fleet state".into()))?;
    head.verify_signature()?;
    if head.payload.epoch <= pin.epoch {
        return Err(nothing());
    }
    let chain = verify_chain(config_dir, &pin, &head, manifest)?;
    if chain.outcome == Transition::Accepted {
        return Err(nothing());
    }
    let (accepted_hash, accepted_epoch) = match &chain.accepted {
        Some((step, _)) => (step.payload_hash()?, step.payload.epoch),
        None => (pin.payload_hash.clone(), pin.epoch),
    };
    // Never drop an accepted epoch: the proposal must directly follow the
    // newest accepted state.
    if head.payload.prev.as_deref() != Some(accepted_hash.as_str())
        || head.payload.epoch != accepted_epoch + 1
    {
        return Err(MusterError::Security(
            "the pending proposal does not directly follow the newest accepted state".into(),
        ));
    }
    Ok(PendingProposal {
        head,
        accepted_hash,
        accepted_epoch,
        prior: chain.prior,
    })
}

/// Groups whose ciphertext or sidecar differs between two vault baselines.
pub fn changed_groups(
    old_vault: &BTreeMap<String, String>,
    old_meta: &BTreeMap<String, String>,
    new_vault: &BTreeMap<String, String>,
    new_meta: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut groups: Vec<String> = Vec::new();
    for (old, new) in [(old_vault, new_vault), (old_meta, new_meta)] {
        for name in old.keys().chain(new.keys()) {
            if old.get(name) != new.get(name) && !groups.contains(name) {
                groups.push(name.clone());
            }
        }
    }
    groups.sort();
    groups
}

/// A signer that is not an admin may only touch groups it reads: its machine's
/// age recipient must be among the group's recipients.
pub fn ensure_reads_groups(
    manifest: &Manifest,
    signer: &str,
    groups: &[String],
) -> Result<(), MusterError> {
    let recipient = manifest
        .machines
        .values()
        .find(|m| m.signing_key.as_deref() == Some(signer))
        .map(|m| m.recipient.clone())
        .filter(|r| !manifest.revoked_recipients.contains(r));
    for group in groups {
        let reads = recipient
            .as_ref()
            .is_some_and(|r| crate::team::group_recipients(manifest, group).contains(r));
        if !reads {
            return Err(MusterError::Security(format!(
                "fleet state signer {} changed vault group '{group}' but is neither an admin \
                 nor a reader of that group",
                signing::fingerprint(signer)
            )));
        }
    }
    Ok(())
}

fn member_active_admin(old: &Value, member: &str, machine: &str, key: &str) -> bool {
    let m = &old["members"][member];
    m["role"] == "admin"
        && m["status"] == "active"
        && m["machines"]
            .as_array()
            .is_some_and(|ms| ms.iter().any(|x| x == machine))
        && old["machines"][machine]["signing_key"] == key
}

/// The invitation an approval is given for: role and inviter (so an approval
/// of a plain member cannot promote to admin), plus the invitation's random
/// nonce and the state it was created in (so an approval is single-use and
/// never verifies against a later re-invitation).
#[derive(Clone, Copy)]
pub struct Invitation<'a> {
    pub role: &'a Value,
    pub invited_by: &'a Value,
    pub nonce: &'a Value,
    pub state: &'a Value,
}

/// Message an admin signs to approve a pending member.
pub fn member_approval_message(
    name: &str,
    invitation: Invitation<'_>,
) -> Result<Vec<u8>, MusterError> {
    canonical_bytes(&json!({
        "domain": "muster member approval v2",
        "member": name,
        "role": invitation.role,
        "invited_by": invitation.invited_by,
        "invite_nonce": invitation.nonce,
        "invited_at_state": invitation.state,
    }))
}

/// A signed member approval as stored in `Member.approvals`:
/// `<admin>:<machine>:<signature hex>`.
pub fn sign_member_approval(
    admin: &str,
    machine: &str,
    member: &str,
    invitation: Invitation<'_>,
    signer: &Signer,
) -> Result<String, MusterError> {
    let message = member_approval_message(member, invitation)?;
    Ok(format!("{admin}:{machine}:{}", signer.sign(&message)))
}

/// Distinct admins (active in `snapshot`) whose entries in `approvals` verify.
/// Bare names and anything malformed never count.
pub fn valid_member_approvals(
    snapshot: &Value,
    member: &str,
    invitation: Invitation<'_>,
    approvals: &[String],
) -> Vec<String> {
    let Ok(message) = member_approval_message(member, invitation) else {
        return Vec::new();
    };
    let mut valid: Vec<String> = Vec::new();
    for entry in approvals {
        let mut parts = entry.splitn(3, ':');
        let (Some(admin), Some(machine), Some(signature)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Some(key) = snapshot["machines"][machine]["signing_key"].as_str() else {
            continue;
        };
        if admin != member
            && member_active_admin(snapshot, admin, machine, key)
            && !valid.iter().any(|a| a == admin)
            && signing::verify(key, &message, signature).is_ok()
        {
            valid.push(admin.to_string());
        }
    }
    valid
}

fn valid_enrollment_approvals(old: &Value, evidence: &EnrollmentEvidence, state: &str) -> usize {
    let request = crate::fleet::EnrollmentRequest {
        name: evidence.name.clone(),
        recipient: evidence.recipient.clone(),
        signing_key: evidence.signing_key.clone(),
        nonce: evidence.nonce.clone(),
        platform: String::new(),
        muster_version: String::new(),
        agents: Vec::new(),
        created_at: evidence.created_at.clone(),
        approvals: Vec::new(),
        role: evidence.role.clone(),
        profile: evidence.profile.clone(),
        member: evidence.member.clone(),
    };
    let Ok(message) = request.approval_message(Some(state)) else {
        return 0;
    };
    let mut seen: Vec<&str> = Vec::new();
    for approval in &evidence.approvals {
        if member_active_admin(
            old,
            &approval.member,
            &approval.machine,
            &approval.signing_key,
        ) && !seen.contains(&approval.member.as_str())
            && signing::verify(&approval.signing_key, &message, &approval.signature).is_ok()
        {
            seen.push(&approval.member);
        }
    }
    seen.len()
}

/// When the team requires several admins (`required_approvals > 1`), every
/// machine or member a governance change adds must carry that many signed
/// admin approvals. The recovery key is break-glass and stands alone.
fn check_quorum(
    old: &Value,
    new: &Value,
    state: &FleetState,
    prior_hash: &str,
) -> Result<(), MusterError> {
    let required = old["required_approvals"].as_u64().unwrap_or(1) as usize;
    let team_mode = old["members"]
        .as_object()
        .is_some_and(|members| !members.is_empty());
    let by_recovery =
        old["recovery"]["signing_key"].as_str() == Some(state.payload.signer.as_str());
    if !by_recovery {
        // A revoked machine or key never comes back through an enrollment,
        // whatever approvals it carries; un-revoking is a separate governance
        // change that needs the full quorum.
        if let Some(name) = revoked_machine_added(old, new) {
            return Err(MusterError::Security(format!(
                "governance change re-adds machine '{name}' whose age recipient or signing key \
                 was revoked; revocation is permanent"
            )));
        }
    }
    if required <= 1 || !team_mode || by_recovery {
        return Ok(());
    }
    let short = |what: String, found: usize| {
        MusterError::Security(format!(
            "governance change {what} without a quorum of {required} signed admin approvals \
             (found {found}); bare names never count"
        ))
    };
    if let Some(members) = new["members"].as_object() {
        let mut seen = std::collections::BTreeSet::new();
        for member in members.values() {
            if let Some(ms) = member["machines"].as_array() {
                for m in ms {
                    if let Some(m_str) = m.as_str() {
                        if !seen.insert(m_str) {
                            return Err(MusterError::Security(format!(
                                "machine '{m_str}' is assigned to multiple members in governance"
                            )));
                        }
                    }
                }
            }
        }
    }
    if let Some(machines) = new["machines"].as_object() {
        for (name, machine) in machines {
            if old["machines"].get(name).is_some() {
                continue;
            }
            let evidence = state.payload.enrollments.iter().find(|e| {
                e.name == *name
                    && Value::String(e.recipient.clone()) == machine["recipient"]
                    && e.signing_key.as_deref() == machine["signing_key"].as_str()
            });
            let found = evidence
                .map(|e| valid_enrollment_approvals(old, e, prior_hash))
                .unwrap_or(0);
            if found < required {
                return Err(short(format!("adds machine '{name}'"), found));
            }
            if team_mode {
                let Some(e) = evidence else {
                    return Err(short(format!("adds machine '{name}'"), 0));
                };
                let Some(member_name) = e.member.as_deref() else {
                    return Err(MusterError::Security(format!(
                        "machine '{name}' enrollment evidence specifies no member in team mode"
                    )));
                };
                let Some(members) = new["members"].as_object() else {
                    return Err(MusterError::Security(format!(
                        "machine '{name}' enrolled but no members exist in governance"
                    )));
                };
                let Some(member) = members.get(member_name) else {
                    return Err(MusterError::Security(format!(
                        "machine '{name}' enrollment evidence approved member '{member_name}' which does not exist in governance"
                    )));
                };
                let has_m = member["machines"]
                    .as_array()
                    .is_some_and(|ms| ms.iter().any(|x| x.as_str() == Some(name)));
                if !has_m {
                    return Err(MusterError::Security(format!(
                        "machine '{name}' enrollment evidence approved member '{member_name}' but machine is not assigned to that member (orphan machine)"
                    )));
                }
                if let Some(erole) = &e.role {
                    if member["role"].as_str() != Some(erole) {
                        return Err(MusterError::Security(format!(
                            "machine '{name}' enrollment evidence approved role '{erole}' but member '{member_name}' has role '{:?}'",
                            member["role"].as_str()
                        )));
                    }
                }
            }
        }
    }
    if let Some(members) = new["members"].as_object() {
        // A brand-new invitation is minted against the accepted state, with a
        // fresh nonce: copying an earlier invitation's fields cannot revive
        // its approvals.
        for (name, member) in members {
            if old["members"][name].is_object() || member["status"] == "active" {
                continue;
            }
            if member["invited_at_state"].as_str() != Some(prior_hash)
                || !member["invite_nonce"]
                    .as_str()
                    .is_some_and(|n| !n.is_empty())
            {
                return Err(MusterError::Security(format!(
                    "invites member '{name}' without a fresh nonce bound to the accepted state"
                )));
            }
        }
        for (name, member) in members {
            let was_active = old["members"][name]["status"] == "active";
            if member["status"] != "active" || was_active {
                continue;
            }
            let approvals: Vec<String> = member["approvals"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let is_text = |v: &Value| v.as_str().is_some_and(|s| !s.is_empty());
            if !is_text(&member["invite_nonce"]) || !is_text(&member["invited_at_state"]) {
                return Err(MusterError::Security(format!(
                    "activates member '{name}' whose invitation carries no nonce and creation \
                     state; re-invite the member"
                )));
            }
            // The approvals must belong to the invitation the prior state
            // holds, not to an earlier one that was dropped and re-created.
            let old_member = &old["members"][name];
            let same_invitation = if old_member.is_object() {
                old_member["invite_nonce"] == member["invite_nonce"]
                    && old_member["invited_at_state"] == member["invited_at_state"]
            } else {
                member["invited_at_state"].as_str() == Some(prior_hash)
            };
            if !same_invitation {
                return Err(MusterError::Security(format!(
                    "activates member '{name}' with an invitation that is not the one the \
                     accepted state holds; approvals of an earlier invitation do not carry over"
                )));
            }
            if let Some(machine) = revoked_member_machine(old, new, member) {
                return Err(MusterError::Security(format!(
                    "activates member '{name}' who owns machine '{machine}' whose age \
                     recipient or signing key was revoked; revocation is permanent"
                )));
            }
            let found = valid_member_approvals(
                old,
                name,
                Invitation {
                    role: &member["role"],
                    invited_by: &member["invited_by"],
                    nonce: &member["invite_nonce"],
                    state: &member["invited_at_state"],
                },
                &approvals,
            )
            .len();
            if found < required {
                return Err(short(format!("activates member '{name}'"), found));
            }
        }
    }
    Ok(())
}

/// What [`verify`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// No `fleet.state.json` and no pin: a repo from before signed state.
    Legacy,
    Verified {
        epoch: u64,
        signer: String,
        /// The pin was created by this call (trust on first use).
        first_use: bool,
    },
    /// A governance change signed by fewer admins than the last accepted state
    /// requires. Not accepted: the pin does not move and security decisions
    /// stay on the last accepted state until enough admins co-sign.
    Pending {
        epoch: u64,
        signer: String,
        received: usize,
        required: usize,
        /// Governance diff against the last accepted state.
        diff: String,
    },
}

/// Verifies the repository against the local pin and advances the pin.
///
/// Refused with `Security`: a missing state while a pin exists (downgrade);
/// a bad signature; a signer that the last accepted state does not authorize;
/// a lower epoch (rollback) or a different payload at the same epoch; a
/// governance digest that differs from `muster.toml`; any vault `.age` file
/// forged, replaced, missing or extra. When several epochs were skipped the
/// chain is not walked: the signer only has to be authorized under the last
/// accepted state (see the ADR).
pub fn verify(
    config_dir: &Path,
    state_dir: &Path,
    manifest: &Manifest,
    expect_signer: Option<&str>,
) -> Result<Verification, MusterError> {
    verify_with(
        config_dir,
        state_dir,
        manifest,
        &VerifyOptions {
            expect_signer,
            persist: true,
            check_content: true,
        },
    )
}

/// How [`verify_with`] behaves.
#[derive(Debug, Clone, Copy)]
pub struct VerifyOptions<'a> {
    pub expect_signer: Option<&'a str>,
    /// Advance (or create) the local pin on success.
    pub persist: bool,
    /// Compare `muster.toml` and the vault with the signed digests. Off only
    /// for `muster trust resign`, which exists to sign a changed repo.
    pub check_content: bool,
}

pub fn verify_with(
    config_dir: &Path,
    state_dir: &Path,
    manifest: &Manifest,
    options: &VerifyOptions,
) -> Result<Verification, MusterError> {
    let expect_signer = options.expect_signer;
    let anchor = TrustAnchor::load(state_dir)?;
    let Some(state) = FleetState::load(config_dir)? else {
        if let Some(expected) = expect_signer {
            if anchor.is_none() {
                return Err(MusterError::Security(format!(
                    "expected signed fleet state from {expected}, but the remote repository is \
                     unsigned"
                )));
            }
        }
        return if anchor.is_some() {
            Err(MusterError::Security(
                "fleet.state.json is missing from the config repo but this machine has accepted \
                 a signed state before (possible downgrade attack); restore the repo from a \
                 trusted copy"
                    .into(),
            ))
        } else {
            Ok(Verification::Legacy)
        };
    };
    state.verify_signature()?;
    let signer = state.payload.signer.clone();
    let hash = state.payload_hash()?;
    let mut outcome = Transition::Accepted;
    // The latest governance change is short of its co-signatures: the newest
    // quorate state in the history, to pin instead of this one (a machine
    // with no pin takes it from genesis, a pinned one from its pin forward).
    let mut bootstrap: Option<(FleetState, Manifest)> = None;

    match &anchor {
        Some(pin) => {
            if state.payload.epoch < pin.epoch {
                return Err(MusterError::Security(format!(
                    "rollback refused: the repo has fleet state epoch {} but this machine already \
                     accepted epoch {}; nothing was applied",
                    state.payload.epoch, pin.epoch
                )));
            }
            if state.payload.epoch == pin.epoch && hash != pin.payload_hash {
                return Err(MusterError::Security(format!(
                    "fleet state at epoch {} differs from the one this machine accepted",
                    pin.epoch
                )));
            }
            if state.payload.epoch > pin.epoch {
                let chain = verify_chain(config_dir, pin, &state, manifest)?;
                outcome = chain.outcome;
                bootstrap = chain.accepted;
            }
        }
        None => {
            if let Some(expected) = expect_signer {
                if !signing::signer_matches(expected, &signer) {
                    return Err(MusterError::Security(format!(
                        "fleet state is signed by {} but {expected} was expected",
                        signing::fingerprint(&signer)
                    )));
                }
            }
            // Trust on first use: the state must at least authorize its own signer.
            let (admins, writers) = signer_sets(manifest);
            if !admins.contains(&signer) && !writers.contains(&signer) {
                return Err(MusterError::Security(
                    "fleet state signer is not an enrolled machine of the fleet it describes"
                        .into(),
                ));
            }
            if let Some((held, step, step_manifest)) = bootstrap_hold(config_dir, &state, manifest)?
            {
                outcome = held;
                bootstrap = Some((step, step_manifest));
            }
        }
    }

    let pending = |outcome: Transition| match outcome {
        Transition::Pending { received, required } => {
            let base = match (&anchor, &bootstrap) {
                (_, Some((_, m))) => Some(governance_content(m)),
                (Some(pin), None) => pin.snapshot.clone(),
                _ => None,
            };
            Some(Verification::Pending {
                epoch: state.payload.epoch,
                signer: state.payload.signer.clone(),
                received,
                required,
                diff: governance_diff(base.as_ref(), &governance_content(manifest)),
            })
        }
        Transition::Accepted => None,
    };
    if !options.check_content {
        if let Some(held) = pending(outcome) {
            return Ok(held);
        }
        return Ok(Verification::Verified {
            epoch: state.payload.epoch,
            signer,
            first_use: anchor.is_none(),
        });
    }
    if state.payload.governance != governance_digest(manifest)? {
        return Err(MusterError::Security(
            "security fields of muster.toml (machines, members, groups, recovery, revoked \
             recipients or secret policies) do not match the signed fleet state; if you edited \
             them on purpose run `muster trust resign`, otherwise do not trust this repo"
                .into(),
        ));
    }
    let actual = vault_hashes(config_dir)?;
    for (group, signed) in &state.payload.vault {
        match actual.get(group) {
            Some(h) if h == signed => {}
            Some(_) => {
                return Err(MusterError::Security(format!(
                    "vault group '{group}' does not match the signed fleet state (forged or \
                     replaced ciphertext)"
                )))
            }
            None => {
                return Err(MusterError::Security(format!(
                    "vault group '{group}' is missing but the signed fleet state lists it"
                )))
            }
        }
    }
    if let Some(group) = actual
        .keys()
        .find(|g| !state.payload.vault.contains_key(*g))
    {
        return Err(MusterError::Security(format!(
            "vault group '{group}' is not in the signed fleet state (unsigned ciphertext)"
        )));
    }
    // A state signed before sidecars were covered has no digests: nothing to
    // compare until the next change re-signs it.
    if !(state.payload.vault_meta.is_empty() && !state.payload.vault.is_empty()) {
        let actual_meta = vault_meta_hashes(config_dir)?;
        for (group, signed) in &state.payload.vault_meta {
            match actual_meta.get(group) {
                Some(h) if h == signed => {}
                Some(_) => {
                    return Err(MusterError::Security(format!(
                        "metadata sidecar '{group}.meta.json' does not match the signed fleet \
                         state (tampered recipients or secret names)"
                    )))
                }
                None => {
                    return Err(MusterError::Security(format!(
                        "metadata sidecar '{group}.meta.json' is missing but the signed fleet \
                         state lists it"
                    )))
                }
            }
        }
        if let Some(group) = actual_meta
            .keys()
            .find(|g| !state.payload.vault_meta.contains_key(*g))
        {
            return Err(MusterError::Security(format!(
                "metadata sidecar '{group}.meta.json' is not in the signed fleet state \
                 (unsigned metadata)"
            )));
        }
    }

    // Pending: content was checked above, but the pin stays where it is (a
    // machine with no pin gets the last quorate state, never the pending one).
    if let Some(held) = pending(outcome) {
        if let (true, Some((step, step_manifest))) = (options.persist, &bootstrap) {
            let confirmed = expect_signer.is_some_and(|e| signing::signer_matches(e, &signer));
            let tofu = match &anchor {
                Some(pin) => pin.tofu && !confirmed,
                None => !confirmed,
            };
            TrustAnchor::for_state(step, step_manifest, tofu)?.save(state_dir)?;
        }
        return Ok(held);
    }
    if options.persist {
        // A first-use pin stays unconfirmed until a run names the signer with
        // `--expect-signer`; a pin that was already confirmed stays so.
        let confirmed = expect_signer.is_some_and(|e| signing::signer_matches(e, &signer));
        let tofu = match &anchor {
            Some(pin) => pin.tofu && !confirmed,
            None => !confirmed,
        };
        TrustAnchor::for_state(&state, manifest, tofu)?.save(state_dir)?;
    }
    Ok(Verification::Verified {
        epoch: state.payload.epoch,
        signer,
        first_use: anchor.is_none(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Machine;
    use tempfile::{tempdir, TempDir};

    struct Fixture {
        _dir: TempDir,
        config: PathBuf,
        state: PathBuf,
        manifest: Manifest,
        signer: Signer,
    }

    fn fixture() -> Fixture {
        let dir = tempdir().unwrap();
        let config = dir.path().join("config");
        let state = dir.path().join("state");
        fs::create_dir_all(config.join("vault")).unwrap();
        let signer = Signer::load_or_create(&state).unwrap();
        let mut manifest = Manifest::new();
        manifest.machines.insert(
            "node1".into(),
            Machine {
                recipient: "age1node1".into(),
                signing_key: Some(signer.public()),
                platform: "linux-x86_64".into(),
                muster_version: "0.1.0".into(),
                agents: vec![],
                last_sync: None,
                role: None,
                profile: None,
                applied_profile: None,
            },
        );
        Fixture {
            _dir: dir,
            config,
            state,
            manifest,
            signer,
        }
    }

    impl Fixture {
        fn seal(&self) -> FleetState {
            seal(
                &self.config,
                &self.state,
                &self.manifest,
                &self.signer,
                false,
            )
            .unwrap()
            .unwrap()
        }
        fn verify(&self) -> Result<Verification, MusterError> {
            verify(&self.config, &self.state, &self.manifest, None)
        }
    }

    fn message(err: MusterError) -> String {
        assert!(matches!(err, MusterError::Security(_)), "{err:?}");
        err.to_string()
    }

    #[test]
    fn canonical_encoding_is_stable() {
        let value = json!({"b": [1, "x\n", null, true], "a": {"z": 2, "y": {"k": "é"}}, "": 0});
        assert_eq!(
            String::from_utf8(canonical_bytes(&value).unwrap()).unwrap(),
            r#"{"":0,"a":{"y":{"k":"é"},"z":2},"b":[1,"x\n",null,true]}"#
        );
        assert!(canonical_bytes(&json!({"f": 1.5})).is_err());
    }

    #[test]
    fn key_order_does_not_change_the_bytes() {
        let a: Value = serde_json::from_str(r#"{"x":1,"y":{"a":1,"b":2}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"y":{"b":2,"a":1},"x":1}"#).unwrap();
        assert_eq!(canonical_bytes(&a).unwrap(), canonical_bytes(&b).unwrap());
    }

    #[test]
    fn payload_hash_has_a_golden_vector() {
        let state = FleetState {
            payload: StatePayload {
                version: 1,
                epoch: 7,
                prev: None,
                governance: "g".into(),
                vault: BTreeMap::from([("app".into(), "h".into())]),
                vault_meta: BTreeMap::from([("app".into(), "m".into())]),
                enrollments: vec![],
                signer: "musterpk1ab".into(),
            },
            signature: String::new(),
            cosignatures: Vec::new(),
            rejections: Vec::new(),
        };
        assert_eq!(
            String::from_utf8(state.payload_bytes().unwrap()).unwrap(),
            r#"{"epoch":7,"governance":"g","prev":null,"signer":"musterpk1ab","vault":{"app":"h"},"vault_meta":{"app":"m"},"version":1}"#
        );
        // States signed before sidecar coverage keep their exact bytes.
        let legacy = FleetState {
            payload: StatePayload {
                vault_meta: BTreeMap::new(),
                ..state.payload
            },
            signature: String::new(),
            cosignatures: Vec::new(),
            rejections: Vec::new(),
        };
        assert_eq!(
            String::from_utf8(legacy.payload_bytes().unwrap()).unwrap(),
            r#"{"epoch":7,"governance":"g","prev":null,"signer":"musterpk1ab","vault":{"app":"h"},"version":1}"#
        );
    }

    #[test]
    fn seal_then_verify_pins_and_chains_epochs() {
        let f = fixture();
        assert_eq!(f.verify().unwrap(), Verification::Legacy);
        let first = f.seal();
        assert_eq!((first.payload.epoch, first.payload.prev.clone()), (1, None));
        assert_eq!(
            f.verify().unwrap(),
            Verification::Verified {
                epoch: 1,
                signer: f.signer.public(),
                first_use: false
            }
        );
        // Nothing changed: no new epoch.
        assert!(seal(&f.config, &f.state, &f.manifest, &f.signer, false)
            .unwrap()
            .is_none());
        fs::write(f.config.join("vault/app.age"), b"cipher").unwrap();
        let second = f.seal();
        assert_eq!(second.payload.epoch, 2);
        assert_eq!(second.payload.prev, Some(first.payload_hash().unwrap()));
        f.verify().unwrap();
    }

    #[test]
    fn seal_refuses_when_current_epoch_is_behind_pinned() {
        let f = fixture();
        let first = f.seal();
        assert_eq!(first.payload.epoch, 1);
        fs::write(f.config.join("vault/app.age"), b"cipher").unwrap();
        let second = f.seal();
        assert_eq!(second.payload.epoch, 2);
        first.save(&f.config).unwrap();
        let err = seal(&f.config, &f.state, &f.manifest, &f.signer, true).unwrap_err();
        assert!(message(err).contains("cannot re-sign a rolled-back fleet state"));
    }

    #[test]
    fn rollback_to_an_older_signed_state_is_refused() {
        let f = fixture();
        let old = f.seal();
        fs::write(f.config.join("vault/app.age"), b"cipher").unwrap();
        f.seal();
        old.save(&f.config).unwrap();
        fs::remove_file(f.config.join("vault/app.age")).unwrap();
        assert!(message(f.verify().unwrap_err()).contains("rollback"));
    }

    #[test]
    fn deleted_state_with_a_pin_is_a_downgrade() {
        let f = fixture();
        f.seal();
        fs::remove_file(FleetState::path(&f.config)).unwrap();
        assert!(message(f.verify().unwrap_err()).contains("downgrade"));
    }

    #[test]
    fn forged_replaced_missing_and_extra_ciphertext_name_the_group() {
        let f = fixture();
        let age = f.config.join("vault/prod.age");
        fs::write(&age, b"real").unwrap();
        f.seal();
        fs::write(&age, b"crafted from public keys").unwrap();
        assert!(message(f.verify().unwrap_err()).contains("'prod'"));
        fs::remove_file(&age).unwrap();
        assert!(message(f.verify().unwrap_err()).contains("'prod'"));
        fs::write(&age, b"real").unwrap();
        fs::write(f.config.join("vault/extra.age"), b"x").unwrap();
        assert!(message(f.verify().unwrap_err()).contains("'extra'"));
    }

    #[test]
    fn tampered_missing_and_extra_sidecars_are_refused() {
        let f = fixture();
        let meta = f.config.join("vault/prod.meta.json");
        fs::write(f.config.join("vault/prod.age"), b"real").unwrap();
        fs::write(&meta, br#"{"recipients":["age1a"]}"#).unwrap();
        let state = f.seal();
        assert!(state.payload.vault_meta.contains_key("prod"));
        f.verify().unwrap();
        fs::write(&meta, br#"{"recipients":["age1a","age1evil"]}"#).unwrap();
        let msg = message(f.verify().unwrap_err());
        assert!(
            msg.contains("prod.meta.json") && msg.contains("does not match"),
            "{msg}"
        );
        fs::remove_file(&meta).unwrap();
        assert!(message(f.verify().unwrap_err()).contains("prod.meta.json"));
        fs::write(&meta, br#"{"recipients":["age1a"]}"#).unwrap();
        f.verify().unwrap();
        fs::write(f.config.join("vault/ghost.meta.json"), b"{}").unwrap();
        assert!(message(f.verify().unwrap_err()).contains("ghost.meta.json"));
    }

    #[test]
    fn state_signed_before_sidecar_coverage_still_verifies() {
        let f = fixture();
        fs::write(f.config.join("vault/prod.age"), b"real").unwrap();
        fs::write(f.config.join("vault/prod.meta.json"), b"{}").unwrap();
        let mut payload = f.seal().payload;
        payload.vault_meta.clear();
        payload.epoch = 1;
        let old = FleetState::sign(payload, &f.signer).unwrap();
        old.save(&f.config).unwrap();
        TrustAnchor::for_state(&old, &f.manifest, false)
            .unwrap()
            .save(&f.state)
            .unwrap();
        f.verify().unwrap();
        // The next change re-signs with sidecar digests.
        let next = f.seal();
        assert!(next.payload.vault_meta.contains_key("prod"));
    }

    #[test]
    fn a_pinned_sidecar_digest_set_cannot_be_dropped_later() {
        let f = fixture();
        fs::write(f.config.join("vault/prod.age"), b"real").unwrap();
        fs::write(f.config.join("vault/prod.meta.json"), b"{}").unwrap();
        let first = f.seal();
        assert!(!first.payload.vault_meta.is_empty());
        f.verify().unwrap();
        // The next epoch is validly signed but omits every sidecar digest, so
        // the sidecars would go unchecked ("legacy" window).
        let mut payload = first.payload.clone();
        payload.epoch = 2;
        payload.prev = Some(first.payload_hash().unwrap());
        payload.vault_meta.clear();
        FleetState::sign(payload, &f.signer)
            .unwrap()
            .save(&f.config)
            .unwrap();
        fs::write(f.config.join("vault/prod.meta.json"), b"{\"forged\":1}").unwrap();
        let err = message(f.verify().unwrap_err());
        assert!(err.contains("drops the signed metadata sidecar"), "{err}");
    }

    #[test]
    fn governance_edit_is_refused_but_other_fields_are_free() {
        let mut f = fixture();
        f.seal();
        f.manifest.machines.get_mut("node1").unwrap().last_sync = Some("now".into());
        f.manifest
            .machines
            .get_mut("node1")
            .unwrap()
            .applied_profile = Some("d".into());
        f.verify().unwrap();
        f.manifest.machines.get_mut("node1").unwrap().recipient = "age1evil".into();
        assert!(message(f.verify().unwrap_err()).contains("muster trust resign"));
    }

    #[test]
    fn tampered_state_and_unauthorized_signers_are_refused() {
        let f = fixture();
        let mut state = f.seal();
        state.payload.epoch = 99;
        state.save(&f.config).unwrap();
        assert!(message(f.verify().unwrap_err()).contains("signature"));

        // A stranger's validly signed next epoch is refused under the pin.
        let f = fixture();
        f.seal();
        let stranger = Signer::from_recovery(&crate::identity::Identity::generate()).unwrap();
        let other = tempdir().unwrap();
        seal(&f.config, other.path(), &f.manifest, &stranger, true).unwrap();
        assert!(message(f.verify().unwrap_err()).contains("not authorized"));
    }

    #[test]
    fn auto_first_use_pin_is_unconfirmed_until_the_signer_is_named() {
        let f = fixture();
        f.seal();
        // The machine that signed the state confirmed it by definition.
        assert!(!TrustAnchor::load(&f.state).unwrap().unwrap().tofu);

        let fresh = tempdir().unwrap();
        verify(&f.config, fresh.path(), &f.manifest, None).unwrap();
        assert!(TrustAnchor::load(fresh.path()).unwrap().unwrap().tofu);
        // Advancing the pin keeps it unconfirmed...
        verify(&f.config, fresh.path(), &f.manifest, None).unwrap();
        assert!(TrustAnchor::load(fresh.path()).unwrap().unwrap().tofu);
        // ...until a run names the signer.
        let fp = signing::fingerprint(&f.signer.public());
        verify(&f.config, fresh.path(), &f.manifest, Some(&fp)).unwrap();
        assert!(!TrustAnchor::load(fresh.path()).unwrap().unwrap().tofu);

        let named = tempdir().unwrap();
        verify(&f.config, named.path(), &f.manifest, Some(&fp)).unwrap();
        assert!(!TrustAnchor::load(named.path()).unwrap().unwrap().tofu);
    }

    #[test]
    fn first_use_pins_and_expect_signer_is_strict() {
        let f = fixture();
        f.seal();
        let fresh = tempdir().unwrap();
        let wrong = verify(&f.config, fresh.path(), &f.manifest, Some("SHA256:0000"));
        assert!(message(wrong.unwrap_err()).contains("was expected"));
        let fp = signing::fingerprint(&f.signer.public());
        let ok = verify(&f.config, fresh.path(), &f.manifest, Some(&fp)).unwrap();
        assert!(matches!(
            ok,
            Verification::Verified {
                first_use: true,
                ..
            }
        ));
        assert!(TrustAnchor::load(fresh.path()).unwrap().is_some());
    }

    #[test]
    fn provider_destinations_and_mcp_reveal_are_governance() {
        use crate::adapters::config::{AiProvider, AiProviderKind};
        let mut f = fixture();
        f.manifest.ai_providers.insert(
            "gw".into(),
            AiProvider {
                kind: AiProviderKind::Anthropic,
                base_url: Some("https://gateway.example".into()),
                default_model: None,
                key: Some("native://app/key".into()),
                api_key_helper: None,
            },
        );
        let base = governance_digest(&f.manifest).unwrap();
        f.manifest.ai_providers.get_mut("gw").unwrap().base_url =
            Some("https://evil.example".into());
        let moved = governance_digest(&f.manifest).unwrap();
        assert_ne!(base, moved);
        f.manifest.ai_providers.get_mut("gw").unwrap().key = Some("native://app/other".into());
        assert_ne!(moved, governance_digest(&f.manifest).unwrap());

        let before = governance_digest(&f.manifest).unwrap();
        f.manifest.mcp_server = Some(crate::manifest::McpServerConfig {
            reveal: vec!["native://app/key".into()],
        });
        assert_ne!(before, governance_digest(&f.manifest).unwrap());
    }

    #[test]
    fn explicit_empty_allow_differs_from_absent_policy() {
        use crate::manifest::SecretPolicy;
        let mut f = fixture();
        f.manifest
            .secrets
            .insert("native://app/k".into(), SecretPolicy::default());
        let absent = governance_digest(&f.manifest).unwrap();
        f.manifest.secrets.get_mut("native://app/k").unwrap().allow = Some(vec![]);
        assert_ne!(absent, governance_digest(&f.manifest).unwrap());
    }

    // ---- chain walk, per-group authorization and quorum --------------------

    use crate::fleet::EnrollmentRequest;
    use crate::manifest::{Group, Member, Readers, TeamConfig};

    fn git_in(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn machine_for(signer: &Signer, recipient: &str) -> Machine {
        Machine {
            recipient: recipient.into(),
            signing_key: Some(signer.public()),
            platform: "linux-x86_64".into(),
            muster_version: "0.1.0".into(),
            agents: vec![],
            last_sync: None,
            role: None,
            profile: None,
            applied_profile: None,
        }
    }

    fn new_signer() -> (TempDir, Signer) {
        let dir = tempdir().unwrap();
        let signer = Signer::load_or_create(dir.path()).unwrap();
        (dir, signer)
    }

    impl Fixture {
        /// Writes the manifest and commits the config repo (history for the walk).
        fn commit(&self) {
            self.manifest.save(&self.config).unwrap();
            git_in(&self.config, &["add", "-A"]);
            git_in(
                &self.config,
                &["commit", "-q", "--allow-empty", "-m", "state"],
            );
        }
        /// Seals as the fixture signer and makes the result the accepted
        /// baseline of the peer (`self.state`), as if every admin had agreed.
        fn seal_adopted(&self, evidence: Vec<EnrollmentEvidence>) {
            self.seal_as(&self.signer, tempdir().unwrap().path(), evidence);
            let state = FleetState::load(&self.config).unwrap().unwrap();
            TrustAnchor::for_state(&state, &self.manifest, false)
                .unwrap()
                .save(&self.state)
                .unwrap();
        }
        /// Seals as `signer` with a scratch pin so `self.state` (the peer's
        /// pin) stays where it was.
        fn seal_as(&self, signer: &Signer, scratch: &Path, evidence: Vec<EnrollmentEvidence>) {
            self.manifest.save(&self.config).unwrap();
            seal_with(
                &self.config,
                scratch,
                &self.manifest,
                signer,
                false,
                evidence,
            )
            .unwrap()
            .unwrap();
            self.commit();
        }
    }

    impl Fixture {
        /// A governance change sealed by alice alone (pending under k=2).
        fn seal_pending_revocation(&self) -> FleetState {
            let mut manifest = self.manifest.clone();
            manifest.revoked_recipients.push("age1gone".into());
            manifest.save(&self.config).unwrap();
            seal_with(
                &self.config,
                tempdir().unwrap().path(),
                &manifest,
                &self.signer,
                false,
                vec![],
            )
            .unwrap()
            .unwrap()
        }
    }

    fn jump_fixture(forger_is_revoked: bool) -> Fixture {
        let mut f = fixture();
        git_in(&f.config, &["init", "-q"]);
        let (_k2dir, k2) = new_signer();
        f.manifest
            .machines
            .insert("node2".into(), machine_for(&k2, "age1node2"));
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        // The peer accepts epoch 1 (both machines are admins in single-person mode).
        f.verify().unwrap();
        // Epoch 2: the admin revokes node2.
        f.manifest.machines.remove("node2");
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        // Epoch 3 adds a rogue machine, signed by the revoked key or the real admin.
        f.manifest
            .machines
            .insert("evil".into(), machine_for(&k2, "age1evil"));
        let forger = if forger_is_revoked { &k2 } else { &f.signer };
        f.seal_as(forger, tempdir().unwrap().path(), vec![]);
        f
    }

    #[test]
    fn epoch_jump_signed_by_a_revoked_admin_is_refused() {
        let f = jump_fixture(true);
        let msg = message(f.verify().unwrap_err());
        assert!(
            msg.contains("epoch jump without valid intermediate chain"),
            "{msg}"
        );
    }

    #[test]
    fn epoch_jump_through_a_valid_chain_is_accepted() {
        let f = jump_fixture(false);
        f.verify().unwrap();
    }

    #[test]
    fn epoch_jump_with_missing_history_is_refused() {
        let f = fixture();
        f.seal();
        let first = FleetState::load(&f.config).unwrap().unwrap();
        // Fabricate epoch 3 (valid signature, prev = epoch 1's hash): no repo history.
        let payload = StatePayload {
            epoch: 3,
            prev: Some(first.payload_hash().unwrap()),
            ..first.payload
        };
        FleetState::sign(payload, &f.signer)
            .unwrap()
            .save(&f.config)
            .unwrap();
        let msg = message(f.verify().unwrap_err());
        assert!(
            msg.contains("epoch jump without valid intermediate chain"),
            "{msg}"
        );
    }

    fn team_fixture() -> (Fixture, TempDir, Signer) {
        let mut f = fixture();
        git_in(&f.config, &["init", "-q"]);
        let (k2dir, k2) = new_signer();
        f.manifest
            .machines
            .insert("node2".into(), machine_for(&k2, "age1node2"));
        f.manifest.members.insert(
            "alice".into(),
            Member {
                role: Role::Admin,
                status: MemberStatus::Active,
                machines: vec!["node1".into()],
                ..Member::default()
            },
        );
        f.manifest.members.insert(
            "bob".into(),
            Member {
                role: Role::Member,
                status: MemberStatus::Active,
                machines: vec!["node2".into()],
                ..Member::default()
            },
        );
        for (group, member) in [("ops", "alice"), ("dev", "bob")] {
            f.manifest.groups.insert(
                group.into(),
                Group {
                    readers: Readers {
                        members: vec![member.into()],
                        ..Readers::default()
                    },
                    profile: None,
                },
            );
            fs::write(f.config.join(format!("vault/{group}.age")), b"v1").unwrap();
            fs::write(f.config.join(format!("vault/{group}.meta.json")), b"{}").unwrap();
        }
        f.seal_adopted(vec![]);
        f.verify().unwrap();
        (f, k2dir, k2)
    }

    #[test]
    fn member_machine_cannot_sign_a_vault_change_to_a_group_it_does_not_read() {
        let (f, _k, bob) = team_fixture();
        let scratch = tempdir().unwrap();
        fs::write(f.config.join("vault/ops.age"), b"tampered").unwrap();
        f.seal_as(&bob, scratch.path(), vec![]);
        let msg = message(f.verify().unwrap_err());
        assert!(msg.contains("'ops'") && msg.contains("reader"), "{msg}");
    }

    #[test]
    fn member_machine_may_sign_a_vault_change_to_a_group_it_reads() {
        let (f, _k, bob) = team_fixture();
        let scratch = tempdir().unwrap();
        fs::write(f.config.join("vault/dev.age"), b"v2").unwrap();
        f.seal_as(&bob, scratch.path(), vec![]);
        f.verify().unwrap();
    }

    fn quorum_fixture() -> (Fixture, Signer, Signer) {
        let (mut f, k2dir, k2) = team_fixture();
        // Make bob a second admin and require two approvals.
        f.manifest.members.get_mut("bob").unwrap().role = Role::Admin;
        f.manifest.team = Some(TeamConfig {
            required_approvals: 2,
        });
        f.seal_adopted(vec![]);
        f.verify().unwrap();
        drop(k2dir);
        let (_d, k3) = new_signer();
        (f, k2, k3)
    }

    fn evidence(f: &Fixture, k2: &Signer, k3: &Signer, who: &[&str]) -> EnrollmentEvidence {
        evidence_for(f, k2, k3, who, "alice")
    }

    fn evidence_for(
        f: &Fixture,
        k2: &Signer,
        k3: &Signer,
        who: &[&str],
        member: &str,
    ) -> EnrollmentEvidence {
        let mut request = EnrollmentRequest {
            name: "node3".into(),
            recipient: "age1node3".into(),
            signing_key: Some(k3.public()),
            nonce: "n0nce".into(),
            platform: "linux".into(),
            muster_version: "0.1.0".into(),
            agents: vec![],
            created_at: "now".into(),
            approvals: vec![],
            role: None,
            profile: None,
            member: Some(member.into()),
        };
        let at = current_hash(&f.config).unwrap();
        for admin in who {
            match *admin {
                "alice" => request
                    .sign_approval("alice", "node1", &f.signer, at.as_deref())
                    .unwrap(),
                _ => request
                    .sign_approval("bob", "node2", k2, at.as_deref())
                    .unwrap(),
            }
        }
        EnrollmentEvidence {
            name: "node3".into(),
            recipient: request.recipient.clone(),
            signing_key: request.signing_key.clone(),
            member: request.member.clone(),
            role: None,
            profile: None,
            created_at: request.created_at.clone(),
            nonce: request.nonce.clone(),
            approvals: request
                .approvals
                .iter()
                .filter_map(|a| match a {
                    crate::fleet::ApprovalEntry::Signed(s) => Some(s.clone()),
                    crate::fleet::ApprovalEntry::Legacy(_) => None,
                })
                .collect(),
        }
    }

    #[test]
    fn single_admin_adding_a_machine_under_quorum_two_is_refused_on_peers() {
        let (mut f, k2, k3) = quorum_fixture();
        let scratch = tempdir().unwrap();
        f.manifest
            .machines
            .insert("node3".into(), machine_for(&k3, "age1node3"));
        let one = evidence(&f, &k2, &k3, &["alice"]);
        f.seal_as(&f.signer, scratch.path(), vec![one]);
        let msg = message(f.verify().unwrap_err());
        assert!(
            msg.contains("without a quorum of 2 signed admin approvals"),
            "{msg}"
        );
    }

    #[test]
    fn machine_without_any_evidence_is_refused_and_two_signed_approvals_pass() {
        let (mut f, k2, k3) = quorum_fixture();
        let scratch = tempdir().unwrap();
        f.manifest.members.insert(
            "carol".into(),
            Member {
                role: Role::Member,
                status: MemberStatus::Active,
                ..Member::default()
            },
        );
        f.seal_adopted(vec![]);
        f.verify().unwrap();
        f.manifest
            .machines
            .insert("node3".into(), machine_for(&k3, "age1node3"));
        f.manifest
            .members
            .get_mut("carol")
            .unwrap()
            .machines
            .push("node3".into());
        f.seal_as(&f.signer, scratch.path(), vec![]);
        assert!(message(f.verify().unwrap_err()).contains("quorum"));

        // Re-seal (force) the same change carrying two verified approvals.
        let scratch2 = tempdir().unwrap();
        // Rebuild from the accepted pin: reset the repo to the accepted epoch.
        git_in(&f.config, &["reset", "-q", "--hard", "HEAD~1"]);
        let two = evidence_for(&f, &k2, &k3, &["alice", "bob"], "carol");
        f.seal_as(&f.signer, scratch2.path(), vec![two]);
        f.verify().unwrap();
    }

    /// carol invited by alice with the given nonce, created in the accepted state.
    fn invited(f: &Fixture, nonce: &str, status: MemberStatus, approvals: Vec<String>) -> Member {
        Member {
            role: Role::Member,
            status,
            invited_by: Some("alice".into()),
            invite_nonce: Some(nonce.into()),
            invited_at_state: current_hash(&f.config).unwrap(),
            approvals,
            ..Member::default()
        }
    }

    fn approve_as(admin: &str, machine: &str, m: &Member, signer: &Signer) -> String {
        let (role, by) = (json!(m.role), json!(m.invited_by));
        let (nonce, at) = (json!(m.invite_nonce), json!(m.invited_at_state));
        let inv = Invitation {
            role: &role,
            invited_by: &by,
            nonce: &nonce,
            state: &at,
        };
        sign_member_approval(admin, machine, "carol", inv, signer).unwrap()
    }

    #[test]
    fn bare_member_approvals_never_count_but_signed_ones_do() {
        let (mut f, _k2, _k3) = quorum_fixture();
        let scratch = tempdir().unwrap();
        let carol = invited(
            &f,
            "n1",
            MemberStatus::Active,
            vec!["alice".into(), "bob".into()],
        );
        f.manifest.members.insert("carol".into(), carol);
        f.seal_as(&f.signer, scratch.path(), vec![]);
        assert!(message(f.verify().unwrap_err()).contains("activates member 'carol'"));

        git_in(&f.config, &["reset", "-q", "--hard", "HEAD~1"]);
        let scratch2 = tempdir().unwrap();
        // Sign as alice (node1) only -> still short of 2.
        let mut carol = invited(&f, "n1", MemberStatus::Active, vec![]);
        carol.approvals = vec![approve_as("alice", "node1", &carol, &f.signer)];
        f.manifest.members.insert("carol".into(), carol);
        f.seal_as(&f.signer, scratch2.path(), vec![]);
        assert!(message(f.verify().unwrap_err()).contains("found 1"));
    }

    #[test]
    fn stale_member_approvals_do_not_survive_a_re_invitation() {
        let (mut f, k2, _k3) = quorum_fixture();
        // First invitation (nonce n1): both admins approve it, but it is dropped.
        let first = invited(&f, "n1", MemberStatus::Pending, vec![]);
        let stale = vec![
            approve_as("alice", "node1", &first, &f.signer),
            approve_as("bob", "node2", &first, &k2),
        ];
        f.manifest.members.insert("carol".into(), first);
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        f.verify().unwrap();
        f.manifest.members.remove("carol");
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        f.verify().unwrap();

        // Re-inviting with the old nonce is refused outright.
        f.manifest.members.insert(
            "carol".into(),
            invited(&f, "n1", MemberStatus::Pending, vec![]),
        );
        f.manifest
            .members
            .get_mut("carol")
            .unwrap()
            .invited_at_state = Some("00".into());
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        assert!(message(f.verify().unwrap_err()).contains("fresh nonce"));
        git_in(&f.config, &["reset", "-q", "--hard", "HEAD~1"]);

        // A genuine re-invitation (new nonce), then the old approvals pasted in.
        let second = invited(&f, "n2", MemberStatus::Pending, vec![]);
        f.manifest.members.insert("carol".into(), second);
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        f.verify().unwrap();
        let mut replay = f.manifest.members["carol"].clone();
        replay.status = MemberStatus::Active;
        replay.approvals = stale;
        f.manifest.members.insert("carol".into(), replay);
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        assert!(message(f.verify().unwrap_err()).contains("found 0"));
        git_in(&f.config, &["reset", "-q", "--hard", "HEAD~1"]);

        // The honest flow: both admins approve the current invitation.
        let mut honest = f.manifest.members["carol"].clone();
        honest.status = MemberStatus::Pending;
        honest.approvals = vec![];
        honest.approvals = vec![
            approve_as("alice", "node1", &honest, &f.signer),
            approve_as("bob", "node2", &honest, &k2),
        ];
        honest.status = MemberStatus::Active;
        f.manifest.members.insert("carol".into(), honest);
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        assert!(matches!(f.verify().unwrap(), Verification::Verified { .. }));
    }

    #[test]
    fn activating_a_member_with_a_revoked_machine_is_refused() {
        let (mut f, k2, k3) = quorum_fixture();
        // Accepted (bob co-signs): carol is invited and owns node3, whose key
        // is on the revocation list.
        f.manifest
            .machines
            .insert("node3".into(), machine_for(&k3, "age1node3"));
        f.manifest.revoked_signing_keys.push(k3.public());
        let mut carol = invited(&f, "n1", MemberStatus::Pending, vec![]);
        carol.machines = vec!["node3".into()];
        f.manifest.members.insert("carol".into(), carol.clone());
        let ev = evidence_for(&f, &k2, &k3, &["alice", "bob"], "carol");
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![ev]);
        let mut state = FleetState::load(&f.config).unwrap().unwrap();
        state.add_cosignature(&k2).unwrap();
        state.save(&f.config).unwrap();
        f.verify().unwrap();

        let mut active = carol.clone();
        active.approvals = vec![
            approve_as("alice", "node1", &carol, &f.signer),
            approve_as("bob", "node2", &carol, &k2),
        ];
        active.status = MemberStatus::Active;
        f.manifest.members.insert("carol".into(), active);
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        let err = message(f.verify().unwrap_err());
        assert!(err.contains("revoked"), "{err}");
    }

    /// alice and bob are admins at k=2; carol is a plain member who owns node3.
    fn graft_fixture() -> (Fixture, Signer, Signer) {
        let (mut f, k2dir, k2) = team_fixture();
        let (_d3, k3) = new_signer();
        f.manifest
            .machines
            .insert("node3".into(), machine_for(&k3, "age1node3"));
        f.manifest.members.get_mut("bob").unwrap().role = Role::Admin;
        f.manifest.members.insert(
            "carol".into(),
            Member {
                role: Role::Member,
                status: MemberStatus::Active,
                machines: vec!["node3".into()],
                ..Member::default()
            },
        );
        f.manifest.team = Some(TeamConfig {
            required_approvals: 2,
        });
        f.seal_adopted(vec![]);
        f.verify().unwrap();
        drop(k2dir);
        (f, k2, k3)
    }

    #[test]
    fn grafting_an_existing_machine_onto_another_admin_needs_the_quorum() {
        let (mut f, _k2, _k3) = graft_fixture();
        let scratch = tempdir().unwrap();
        f.manifest
            .members
            .get_mut("bob")
            .unwrap()
            .machines
            .push("node3".into());
        f.seal_as(&f.signer, scratch.path(), vec![]);
        // Either held for co-signatures or refused outright, never accepted:
        // carol's machine must not become one of bob's votes.
        match f.verify() {
            Ok(Verification::Pending { required: 2, .. }) | Err(MusterError::Security(_)) => {}
            other => panic!("grafted machine accepted with one signature: {other:?}"),
        }
    }

    #[test]
    fn rekeying_or_reassigning_an_existing_machine_is_not_enrollment() {
        let (mut f, _k2, _k3) = graft_fixture();
        let scratch = tempdir().unwrap();
        // alice alone swaps carol's machine for another one of alice's.
        f.manifest.members.get_mut("carol").unwrap().machines = vec!["node1".into()];
        f.seal_as(&f.signer, scratch.path(), vec![]);
        assert!(!matches!(f.verify(), Ok(Verification::Verified { .. })));
    }

    #[test]
    fn evidence_must_name_the_member_the_machine_joins() {
        let (mut f, k2, k3) = quorum_fixture();
        let scratch = tempdir().unwrap();
        f.manifest
            .machines
            .insert("node3".into(), machine_for(&k3, "age1node3"));
        // The approved request named alice, the manifest gives node3 to bob.
        f.manifest
            .members
            .get_mut("bob")
            .unwrap()
            .machines
            .push("node3".into());
        let two = evidence(&f, &k2, &k3, &["alice", "bob"]);
        f.seal_as(&f.signer, scratch.path(), vec![two]);
        let msg = message(f.verify().unwrap_err());
        assert!(msg.contains("enrollment evidence approved member"), "{msg}");
    }

    #[test]
    fn a_new_machine_with_its_own_quorum_joining_a_plain_member_is_accepted() {
        let (mut f, k2, _k3) = graft_fixture();
        let scratch = tempdir().unwrap();
        let (_d4, k4) = new_signer();
        f.manifest
            .machines
            .insert("node4".into(), machine_for(&k4, "age1node4"));
        f.manifest
            .members
            .get_mut("carol")
            .unwrap()
            .machines
            .push("node4".into());
        let mut request = EnrollmentRequest {
            name: "node4".into(),
            recipient: "age1node4".into(),
            signing_key: Some(k4.public()),
            nonce: "n".into(),
            platform: "linux".into(),
            muster_version: "0.1.0".into(),
            agents: vec![],
            created_at: "now".into(),
            approvals: vec![],
            role: None,
            profile: None,
            member: Some("carol".into()),
        };
        let at = current_hash(&f.config).unwrap();
        request
            .sign_approval("alice", "node1", &f.signer, at.as_deref())
            .unwrap();
        request
            .sign_approval("bob", "node2", &k2, at.as_deref())
            .unwrap();
        let signed = request
            .approvals
            .iter()
            .filter_map(|a| match a {
                crate::fleet::ApprovalEntry::Signed(s) => Some(s.clone()),
                crate::fleet::ApprovalEntry::Legacy(_) => None,
            })
            .collect();
        let ev = EnrollmentEvidence {
            name: "node4".into(),
            recipient: request.recipient.clone(),
            signing_key: request.signing_key.clone(),
            member: request.member.clone(),
            role: None,
            profile: None,
            created_at: request.created_at.clone(),
            nonce: request.nonce.clone(),
            approvals: signed,
        };
        f.seal_as(&f.signer, scratch.path(), vec![ev]);
        assert!(matches!(f.verify().unwrap(), Verification::Verified { .. }));
    }

    #[test]
    fn admin_rewriting_a_group_it_does_not_read_needs_the_quorum() {
        let (f, k2, _k3) = quorum_fixture();
        let scratch = tempdir().unwrap();
        // alice reads `ops` but not `dev`, which only bob reads.
        fs::write(f.config.join("vault/dev.age"), b"forged by one admin").unwrap();
        f.seal_as(&f.signer, scratch.path(), vec![]);
        match f.verify().unwrap() {
            Verification::Pending {
                received: 1,
                required: 2,
                ..
            } => {}
            other => panic!("vault rewrite accepted with one signature: {other:?}"),
        }
        // Bob co-signs: now it is accepted.
        let mut state = FleetState::load(&f.config).unwrap().unwrap();
        state.add_cosignature(&k2).unwrap();
        state.save(&f.config).unwrap();
        assert!(matches!(f.verify().unwrap(), Verification::Verified { .. }));
    }

    #[test]
    fn admin_rewriting_a_group_it_reads_stays_a_single_signature_change() {
        let (f, _k2, _k3) = quorum_fixture();
        let scratch = tempdir().unwrap();
        fs::write(f.config.join("vault/ops.age"), b"alice updates ops").unwrap();
        f.seal_as(&f.signer, scratch.path(), vec![]);
        assert!(matches!(f.verify().unwrap(), Verification::Verified { .. }));
    }

    #[test]
    fn replayed_evidence_reinstating_a_revoked_machine_is_refused_on_peers() {
        let (mut f, k2, k3) = quorum_fixture();

        // 1. node3 was legitimately enrolled with 2 admin approvals
        f.manifest.members.insert(
            "carol".into(),
            Member {
                role: Role::Member,
                status: MemberStatus::Active,
                ..Member::default()
            },
        );
        f.seal_adopted(vec![]);
        f.verify().unwrap();
        f.manifest
            .machines
            .insert("node3".into(), machine_for(&k3, "age1node3"));
        f.manifest
            .members
            .get_mut("carol")
            .unwrap()
            .machines
            .push("node3".into());
        let ev = evidence_for(&f, &k2, &k3, &["alice", "bob"], "carol");
        f.seal_adopted(vec![ev.clone()]);
        f.verify().unwrap();

        // 2. node3 is later revoked with quorum (recorded in revoked_recipients & revoked_signing_keys)
        f.manifest.machines.remove("node3");
        f.manifest
            .members
            .get_mut("carol")
            .unwrap()
            .machines
            .retain(|m| m != "node3");
        f.manifest.revoked_recipients.push("age1node3".to_string());
        f.manifest.revoked_signing_keys.push(k3.public());
        // Quorum-backed revocation (both admins sign or cosign)
        f.manifest.save(&f.config).unwrap();
        let mut state = seal_with(&f.config, &f.state, &f.manifest, &f.signer, false, vec![])
            .unwrap()
            .unwrap();
        state.add_cosignature(&k2).unwrap();
        state.save(&f.config).unwrap();
        f.commit();
        f.verify().unwrap();

        // 3. One compromised admin (alice) attempts to re-add node3 using the old replayed evidence
        f.manifest
            .machines
            .insert("node3".into(), machine_for(&k3, "age1node3"));
        f.manifest
            .members
            .get_mut("carol")
            .unwrap()
            .machines
            .push("node3".into());
        let scratch3 = tempdir().unwrap();
        f.seal_as(&f.signer, scratch3.path(), vec![ev]);

        // Verifier must refuse: revocation is permanent at the verifier
        let err = f.verify().unwrap_err();
        let msg = message(err);
        assert!(
            msg.contains("revoked") && msg.contains("permanent"),
            "{msg}"
        );
    }

    #[test]
    fn approvals_given_against_another_state_do_not_count() {
        let (mut f, k2, k3) = quorum_fixture();
        let scratch = tempdir().unwrap();
        f.manifest
            .machines
            .insert("node3".into(), machine_for(&k3, "age1node3"));
        // Both admins signed, but against a state that is not the one the new
        // epoch chains to.
        let mut ev = evidence(&f, &k2, &k3, &["alice", "bob"]);
        let mut request = EnrollmentRequest {
            name: "node3".into(),
            recipient: "age1node3".into(),
            signing_key: Some(k3.public()),
            nonce: "n0nce".into(),
            platform: "linux".into(),
            muster_version: "0.1.0".into(),
            agents: vec![],
            created_at: "now".into(),
            approvals: vec![],
            role: None,
            profile: None,
            member: Some("alice".into()),
        };
        request
            .sign_approval("alice", "node1", &f.signer, Some("stale"))
            .unwrap();
        request
            .sign_approval("bob", "node2", &k2, Some("stale"))
            .unwrap();
        ev.approvals = request
            .approvals
            .iter()
            .filter_map(|a| match a {
                crate::fleet::ApprovalEntry::Signed(s) => Some(s.clone()),
                crate::fleet::ApprovalEntry::Legacy(_) => None,
            })
            .collect();
        f.seal_as(&f.signer, scratch.path(), vec![ev]);
        let msg = message(f.verify().unwrap_err());
        assert!(msg.contains("without a quorum of 2"), "{msg}");
    }

    #[test]
    fn dropped_invitation_bundling_unread_vault_rewrite_stays_pending() {
        let (mut f, _k2, _k3) = quorum_fixture();
        let scratch = tempdir().unwrap();

        // Add a pending invitation to the fixture state
        f.manifest.members.insert(
            "pending_user".into(),
            Member {
                role: Role::Member,
                status: MemberStatus::Pending,
                invited_by: Some("alice".into()),
                machines: vec![],
                ..Member::default()
            },
        );
        f.seal_adopted(vec![]);
        f.verify().unwrap();

        // Alice drops the pending invitation AND tampers vault/dev.age (which alice does not read)
        f.manifest.members.remove("pending_user");
        fs::write(f.config.join("vault/dev.age"), b"forged_bundled_with_drop").unwrap();

        f.seal_as(&f.signer, scratch.path(), vec![]);
        match f.verify().unwrap() {
            Verification::Pending {
                received: 1,
                required: 2,
                ..
            } => {}
            other => panic!("bundled rewrite accepted with one signature: {other:?}"),
        }
    }

    #[test]
    fn enrollment_bundling_unread_vault_rewrite_stays_pending() {
        let (mut f, k2, _k3) = graft_fixture();
        let scratch = tempdir().unwrap();

        let (_d4, k4) = new_signer();
        f.manifest
            .machines
            .insert("node4".into(), machine_for(&k4, "age1node4"));
        f.manifest
            .members
            .get_mut("carol")
            .unwrap()
            .machines
            .push("node4".into());

        let mut request = EnrollmentRequest {
            name: "node4".into(),
            recipient: "age1node4".into(),
            signing_key: Some(k4.public()),
            nonce: "n".into(),
            platform: "linux".into(),
            muster_version: "0.1.0".into(),
            agents: vec![],
            created_at: "now".into(),
            approvals: vec![],
            role: None,
            profile: None,
            member: Some("carol".into()),
        };
        let at = current_hash(&f.config).unwrap();
        request
            .sign_approval("alice", "node1", &f.signer, at.as_deref())
            .unwrap();
        request
            .sign_approval("bob", "node2", &k2, at.as_deref())
            .unwrap();

        let signed = request
            .approvals
            .iter()
            .filter_map(|a| match a {
                crate::fleet::ApprovalEntry::Signed(s) => Some(s.clone()),
                crate::fleet::ApprovalEntry::Legacy(_) => None,
            })
            .collect();
        let ev = EnrollmentEvidence {
            name: "node4".into(),
            recipient: request.recipient.clone(),
            signing_key: request.signing_key.clone(),
            member: request.member.clone(),
            role: None,
            profile: None,
            created_at: request.created_at.clone(),
            nonce: request.nonce.clone(),
            approvals: signed,
        };

        // Alice rewrites dev.age (unread by alice) while bundling the approved enrollment
        fs::write(
            f.config.join("vault/dev.age"),
            b"forged_bundled_with_enrollment",
        )
        .unwrap();

        f.seal_as(&f.signer, scratch.path(), vec![ev]);
        match f.verify().unwrap() {
            Verification::Pending {
                received: 1,
                required: 2,
                ..
            } => {}
            other => panic!("bundled enrollment rewrite accepted with one signature: {other:?}"),
        }
    }

    /// An unauthorized or junk signature sitting in an older copy of a state
    /// never hides the genuine co-signature of the same admin.
    #[test]
    fn junk_cosignature_in_history_does_not_hide_the_genuine_one() {
        let (mut f, k2, _k3) = quorum_fixture();
        let scratch = tempdir().unwrap();
        f.manifest.revoked_recipients.push("age1gone".into());
        f.seal_as(&f.signer, scratch.path(), vec![]);
        let mut junk = FleetState::load(&f.config).unwrap().unwrap();
        junk.cosignatures.push(Cosignature {
            signer: k2.public(),
            signature: "00".repeat(64),
        });
        junk.save(&f.config).unwrap();
        f.commit();
        let mut genuine = FleetState::load(&f.config).unwrap().unwrap();
        genuine.cosignatures.clear();
        genuine.add_cosignature(&k2).unwrap();
        genuine.save(&f.config).unwrap();
        f.commit();
        f.manifest.revoked_recipients.push("age1gone2".into());
        f.seal_as(&f.signer, scratch.path(), vec![]);
        match f.verify() {
            Ok(Verification::Pending { .. }) => {}
            other => panic!("the genuine co-signature must count: {other:?}"),
        }
    }

    /// A placeholder entry never blocks the real signature from being added.
    #[test]
    fn add_cosignature_replaces_an_invalid_placeholder() {
        let (f, k2, _k3) = quorum_fixture();
        let mut state = f.seal_pending_revocation();
        state.cosignatures.push(Cosignature {
            signer: k2.public(),
            signature: "00".repeat(64),
        });
        assert!(state.valid_cosigners().is_empty());
        state.add_cosignature(&k2).unwrap();
        assert_eq!(state.valid_cosigners(), vec![k2.public()]);
        assert_eq!(state.cosignatures.len(), 1);
    }

    /// In team mode, an approved machine that is not assigned to any member
    /// (an orphan) is refused with Security by peers.
    #[test]
    fn orphan_approved_machine_in_team_mode_is_refused() {
        let (mut f, k2, k3) = quorum_fixture();
        f.manifest.members.insert(
            "carol".into(),
            Member {
                role: Role::Member,
                status: MemberStatus::Active,
                ..Member::default()
            },
        );
        f.seal_adopted(vec![]);
        f.verify().unwrap();

        // One admin finalizes enrollment of node3 with valid evidence naming carol,
        // but omits node3 from carol.machines (leaving it as an orphan).
        f.manifest
            .machines
            .insert("node3".into(), machine_for(&k3, "age1node3"));
        // NOT added to carol.machines!
        let ev = evidence_for(&f, &k2, &k3, &["alice", "bob"], "carol");
        let scratch = tempdir().unwrap();
        f.seal_as(&f.signer, scratch.path(), vec![ev]);

        let err = f.verify().unwrap_err();
        let msg = message(err);
        assert!(
            msg.contains("machine 'node3'")
                && (msg.contains("orphan") || msg.contains("not assigned")),
            "{msg}"
        );
    }

    /// In team mode, an approved machine assigned to a member different from
    /// the one in its enrollment evidence is refused with Security.
    #[test]
    fn machine_assigned_to_wrong_member_is_refused() {
        let (mut f, k2, k3) = quorum_fixture();
        f.manifest.members.insert(
            "carol".into(),
            Member {
                role: Role::Member,
                status: MemberStatus::Active,
                ..Member::default()
            },
        );
        f.manifest.members.insert(
            "dave".into(),
            Member {
                role: Role::Member,
                status: MemberStatus::Active,
                ..Member::default()
            },
        );
        f.seal_adopted(vec![]);
        f.verify().unwrap();

        f.manifest
            .machines
            .insert("node3".into(), machine_for(&k3, "age1node3"));
        f.manifest
            .members
            .get_mut("dave")
            .unwrap()
            .machines
            .push("node3".into());
        let ev = evidence_for(&f, &k2, &k3, &["alice", "bob"], "carol");
        let scratch = tempdir().unwrap();
        f.seal_as(&f.signer, scratch.path(), vec![ev]);

        let err = f.verify().unwrap_err();
        let msg = message(err);
        assert!(
            msg.contains("machine 'node3'") && msg.contains("assigned"),
            "{msg}"
        );
    }

    fn quorum_fixture_pair() -> (Fixture, Signer, u64) {
        let (f, k2, _k3) = quorum_fixture();
        let epoch = TrustAnchor::load(&f.state).unwrap().unwrap().epoch;
        (f, k2, epoch)
    }

    /// A quorate change followed by a lone pending proposal: a peer pinned
    /// before both still takes the quorate one and only holds the pending one.
    fn accepted_then_pending() -> (Fixture, Signer, u64) {
        let (mut f, k2, _k3) = quorum_fixture();
        let start = TrustAnchor::load(&f.state).unwrap().unwrap().epoch;
        let scratch = tempdir().unwrap();
        f.manifest.revoked_recipients.push("age1gone".into());
        f.seal_as(&f.signer, scratch.path(), vec![]);
        let mut state = FleetState::load(&f.config).unwrap().unwrap();
        state.add_cosignature(&k2).unwrap();
        state.save(&f.config).unwrap();
        f.commit();
        f.manifest.revoked_recipients.push("age1gone2".into());
        f.seal_as(&f.signer, scratch.path(), vec![]);
        (f, k2, start)
    }

    #[test]
    fn pin_advances_through_accepted_epochs_while_head_is_pending() {
        let (f, _k2, start) = accepted_then_pending();
        match f.verify() {
            Ok(Verification::Pending { .. }) => {}
            other => panic!("expected the head to be pending: {other:?}"),
        }
        let pin = TrustAnchor::load(&f.state).unwrap().unwrap();
        assert_eq!(pin.epoch, start + 1, "the accepted epoch is pinned");
        assert!(
            pin.snapshot.as_ref().unwrap()["revoked_recipients"]
                .to_string()
                .contains("age1gone"),
            "the quorate revocation is part of the pinned state"
        );
        assert!(
            !pin.snapshot.as_ref().unwrap()["revoked_recipients"]
                .to_string()
                .contains("age1gone2"),
            "the pending proposal is not"
        );
    }

    #[test]
    fn pending_proposal_names_the_newest_accepted_state_not_the_stale_pin() {
        let (f, _k2, start) = accepted_then_pending();
        // The pin is still where the peer left it: it has not verified yet.
        assert_eq!(TrustAnchor::load(&f.state).unwrap().unwrap().epoch, start);
        let proposal = pending_proposal(&f.config, &f.state, &f.manifest).unwrap();
        assert_eq!(proposal.accepted_epoch, start + 1);
        assert_eq!(proposal.head.payload.epoch, start + 2);
    }

    #[test]
    fn pending_proposal_refuses_when_nothing_is_pending() {
        let (f, _k2, _start) = quorum_fixture_pair();
        assert!(pending_proposal(&f.config, &f.state, &f.manifest).is_err());
    }

    /// Sol R12 F2: the local machine never pins a state its peers would reject.
    #[test]
    fn local_machine_never_pins_a_state_peers_would_reject() {
        let (mut f, _k2, k3) = quorum_fixture();
        let pinned = TrustAnchor::load(&f.state).unwrap().unwrap();
        // One admin seals an added machine that carries no approvals at all.
        f.manifest
            .machines
            .insert("node3".into(), machine_for(&k3, "age1node3"));
        f.manifest.save(&f.config).unwrap();
        seal_with(&f.config, &f.state, &f.manifest, &f.signer, false, vec![]).unwrap();
        assert_eq!(TrustAnchor::load(&f.state).unwrap().unwrap(), pinned);
        // Peers refuse the very same state.
        assert!(message(f.verify().unwrap_err()).contains("quorum"));
    }

    mod forged_ciphertext_property {
        use super::*;
        use proptest::prelude::*;

        /// k=2, admins alice (reads `ops`) and bob (reads `dev`), a plain
        /// member carol and a never-active invitation `erin`, all accepted.
        fn base() -> (Fixture, Signer, Signer) {
            let (mut f, k2, k3) = quorum_fixture();
            f.manifest.members.insert(
                "carol".into(),
                Member {
                    role: Role::Member,
                    status: MemberStatus::Active,
                    ..Member::default()
                },
            );
            f.manifest.members.insert(
                "erin".into(),
                invited(
                    &f,
                    "nonce_erin_0123456789abcdef",
                    MemberStatus::Pending,
                    vec![],
                ),
            );
            f.seal_adopted(vec![]);
            f.verify().unwrap();
            (f, k2, k3)
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(32))]

            /// One admin that does not read `dev` signs any mix of exempt
            /// governance edits plus a rewrite of `dev`: never accepted.
            #[test]
            fn one_admin_never_gets_forged_ciphertext_accepted(
                add_invite in any::<bool>(),
                drop_invite in any::<bool>(),
                enroll in any::<bool>(),
                rewrite_meta_only in any::<bool>(),
                also_ops in any::<bool>(),
            ) {
                let (mut f, k2, k3) = base();
                let scratch = tempdir().unwrap();
                let mut evidence_list = vec![];
                if add_invite {
                    f.manifest.members.insert(
                        "frank".into(),
                        invited(&f, "nonce_frank_0123456789abcdef", MemberStatus::Pending, vec![]),
                    );
                }
                if drop_invite {
                    f.manifest.members.remove("erin");
                }
                if enroll {
                    f.manifest
                        .machines
                        .insert("node3".into(), machine_for(&k3, "age1node3"));
                    f.manifest.members.get_mut("carol").unwrap().machines.push("node3".into());
                    evidence_list.push(evidence_for(&f, &k2, &k3, &["alice", "bob"], "carol"));
                }
                if rewrite_meta_only {
                    fs::write(f.config.join("vault/dev.meta.json"), b"{\"forged\":true}").unwrap();
                } else {
                    fs::write(f.config.join("vault/dev.age"), b"forged by one admin").unwrap();
                }
                if also_ops {
                    fs::write(f.config.join("vault/ops.age"), b"alice also updates ops").unwrap();
                }
                f.seal_as(&f.signer, scratch.path(), evidence_list);
                match f.verify() {
                    Ok(Verification::Pending {
                        received: 1,
                        required: 2,
                        ..
                    }) => {}
                    other => prop_assert!(
                        false,
                        "expected Pending {{ received: 1, required: 2 }}, got {:?}",
                        other
                    ),
                }
            }
        }
    }

    /// k=2 fleet whose compromised admin alice signs, alone, a threshold-lowering
    /// epoch 3 (never co-signed) and then a linked epoch 4 that passes under the
    /// lowered threshold in isolation.
    fn lowered_threshold_history() -> Fixture {
        use crate::adapters::config::{AiProvider, AiProviderKind};
        let (mut f, _k2, _k3) = quorum_fixture();
        f.manifest.team = Some(TeamConfig {
            required_approvals: 1,
        });
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        f.manifest.ai_providers.insert(
            "gw".into(),
            AiProvider {
                kind: AiProviderKind::Anthropic,
                base_url: Some("https://evil.example".into()),
                default_model: None,
                key: Some("native://app/key".into()),
                api_key_helper: None,
            },
        );
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        f
    }

    #[test]
    fn fresh_machine_never_pins_past_a_pending_intermediate_state() {
        for expected in [false, true] {
            let f = lowered_threshold_history();
            let fresh = tempdir().unwrap();
            let fp = signing::fingerprint(&f.signer.public());
            let result = verify(
                &f.config,
                fresh.path(),
                &f.manifest,
                expected.then_some(fp.as_str()),
            )
            .unwrap();
            assert!(
                matches!(result, Verification::Pending { required: 2, .. }),
                "expected={expected}: {result:?}"
            );
            let pin = TrustAnchor::load(fresh.path()).unwrap().unwrap();
            assert_eq!(pin.epoch, 2, "only the accepted prefix is pinned");
        }
    }

    #[test]
    fn bootstrap_refuses_a_history_with_no_genesis() {
        let f = lowered_threshold_history();
        // Cut the history so epochs 1..2 are no longer reachable.
        let state = FleetState::load(&f.config).unwrap().unwrap();
        let fresh = tempdir().unwrap();
        git_in(&f.config, &["checkout", "-q", "--orphan", "squashed"]);
        git_in(&f.config, &["add", "-A"]);
        git_in(&f.config, &["commit", "-q", "-m", "squash"]);
        assert_eq!(state.payload.epoch, 4);
        let err = verify(&f.config, fresh.path(), &f.manifest, None).unwrap_err();
        assert!(message(err).contains("missing from the repository history"));
    }

    #[test]
    fn a_junk_fork_committed_first_cannot_block_the_real_chain() {
        let (mut f, k2, _k3) = quorum_fixture();
        // The real epoch 3 (alice, co-signed by bob), committed AFTER a junk
        // self-signed state at the same epoch.
        let (_d, junk) = new_signer();
        let junk_payload = StatePayload {
            epoch: 3,
            prev: Some("0".repeat(64)),
            ..FleetState::load(&f.config).unwrap().unwrap().payload
        };
        let junk_payload = StatePayload {
            signer: junk.public(),
            ..junk_payload
        };
        let junk_state = FleetState::sign(junk_payload, &junk).unwrap();
        let real = FleetState::load(&f.config).unwrap().unwrap();
        junk_state.save(&f.config).unwrap();
        f.commit();
        real.save(&f.config).unwrap();
        f.commit();
        f.manifest.profiles.clear();
        fs::write(f.config.join("vault/ops.age"), b"alice rewrites ops").unwrap();
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        // A machine two epochs behind (pin 2, state 4) walks the history.
        fs::write(f.config.join("vault/ops.age"), b"alice rewrites ops again").unwrap();
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        f.verify().unwrap();
        let _ = k2;
        // And so does a fresh machine.
        let fresh = tempdir().unwrap();
        verify(&f.config, fresh.path(), &f.manifest, None).unwrap();
    }

    #[test]
    fn unauthorized_padding_cosignatures_do_not_beat_the_quorate_version() {
        let (f, k2, _k3) = quorum_fixture();
        // Epoch 3: alice changes governance; bob co-signs. The quorate version
        // is committed, then a later commit re-serializes it padded with
        // unrelated valid keys but without bob.
        let mut f = f;
        f.manifest.team = Some(TeamConfig {
            required_approvals: 2,
        });
        f.manifest.revoked_recipients.push("age1gone".into());
        f.manifest.save(&f.config).unwrap();
        let mut state = seal_with(&f.config, &f.state, &f.manifest, &f.signer, false, vec![])
            .unwrap()
            .unwrap();
        state.add_cosignature(&k2).unwrap();
        state.save(&f.config).unwrap();
        f.commit();
        let quorate = state.clone();
        let mut padded = FleetState {
            cosignatures: Vec::new(),
            ..quorate
        };
        for _ in 0..3 {
            let (_d, extra) = new_signer();
            padded.add_cosignature(&extra).unwrap();
        }
        padded.save(&f.config).unwrap();
        f.commit();
        // Epoch 4 on top (vault-only, alice reads ops).
        quorate_restore(&f, &state);
        fs::write(f.config.join("vault/ops.age"), b"alice updates ops").unwrap();
        f.seal_as(&f.signer, tempdir().unwrap().path(), vec![]);
        f.verify().unwrap();
        let fresh = tempdir().unwrap();
        verify(&f.config, fresh.path(), &f.manifest, None).unwrap();
    }

    /// Sets the working-tree state file back to `state` and commits it.
    fn quorate_restore(f: &Fixture, state: &FleetState) {
        state.save(&f.config).unwrap();
        f.commit();
    }
}
