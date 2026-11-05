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

/// `fleet.state.json`: the payload and its detached signature (hex).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetState {
    pub payload: StatePayload,
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
    pub signer: String,
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

    fn for_state(state: &FleetState, manifest: &Manifest) -> Result<Self, MusterError> {
        let (admins, writers) = signer_sets(manifest);
        Ok(Self {
            epoch: state.payload.epoch,
            payload_hash: state.payload_hash()?,
            governance: state.payload.governance.clone(),
            admins,
            writers,
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
                let list: Vec<String> = p.allow.to_vec();
                *allow = json!(sorted(&list));
            }
            (r.clone(), v)
        })
        .collect();
    json!({
        "machines": machines,
        "members": members,
        "groups": groups,
        "secrets": secrets,
        "required_approvals": manifest.required_approvals(),
        "revoked_recipients": sorted(&manifest.revoked_recipients),
        "recovery": manifest.recovery.as_ref().map(|r| json!({
            "recipient": r.recipient,
            "signing_key": r.signing_key,
        })),
    })
}

pub fn governance_digest(manifest: &Manifest) -> Result<String, MusterError> {
    Ok(sha256_hex(&canonical_bytes(&governance_content(manifest))?))
}

/// `group -> sha256` of every `vault/*.age` file.
pub fn vault_hashes(config_dir: &Path) -> Result<BTreeMap<String, String>, MusterError> {
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
        if path.extension().is_some_and(|e| e == "age") {
            let group = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
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

    fn sign(payload: StatePayload, signer: &Signer) -> Result<Self, MusterError> {
        let mut state = Self {
            payload,
            signature: String::new(),
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
        && state.payload.vault == vault_hashes(config_dir)?)
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
    // A seal must advance or preserve the pinned epoch, never launder an older state.
    ensure_not_behind_pin(config_dir, state_dir, "re-sign")?;
    let current = FleetState::load(config_dir)?;
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
        signer: signer.public(),
    };
    let state = FleetState::sign(payload, signer)?;
    state.save(config_dir)?;
    TrustAnchor::for_state(&state, manifest)?.save(state_dir)?;
    Ok(Some(state))
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

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
                let vault_only = state.payload.governance == pin.governance;
                let allowed = if vault_only {
                    &pin.writers
                } else {
                    &pin.admins
                };
                if !allowed.contains(&signer) {
                    return Err(MusterError::Security(format!(
                        "fleet state signed by {} which is not authorized {} under the last \
                         accepted state",
                        signing::fingerprint(&signer),
                        if vault_only {
                            "to update the vault"
                        } else {
                            "to change governance"
                        }
                    )));
                }
                if state.payload.epoch == pin.epoch + 1
                    && state.payload.prev.as_deref() != Some(pin.payload_hash.as_str())
                {
                    return Err(MusterError::Security(
                        "fleet state history forked: its predecessor is not the accepted state"
                            .into(),
                    ));
                }
            }
        }
        None => {
            if let Some(expected) = expect_signer {
                if expected != signer && expected != signing::fingerprint(&signer) {
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
        }
    }

    if !options.check_content {
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

    if options.persist {
        TrustAnchor::for_state(&state, manifest)?.save(state_dir)?;
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
                signer: "musterpk1ab".into(),
            },
            signature: String::new(),
        };
        assert_eq!(
            String::from_utf8(state.payload_bytes().unwrap()).unwrap(),
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
}
