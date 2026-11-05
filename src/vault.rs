//! Native vault: one age-armored file per secret group, encrypted as a unit
//! for the group's recipients, plus a plaintext metadata sidecar (no values)
//! so `list` works without an identity.
//!
//! Layout under the config repo:
//! - `vault/<group>.age`       armored age ciphertext of [`GroupData`] JSON
//! - `vault/<group>.meta.json` names, timestamps and recipients only

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use secrecy::zeroize::{Zeroize, Zeroizing};
use serde::{Deserialize, Serialize};

use crate::errors::MusterError;
use crate::identity::Identity;
use crate::manifest::{Manifest, VaultPolicy};
use crate::secrets::SecretValue;

const VAULT_DIR: &str = "vault";
const NATIVE_SCHEME: &str = "native://";

/// A secret's metadata. Never carries the value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretMeta {
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
    /// Machine that added / last rotated the secret (unknown for old entries).
    pub created_by: Option<String>,
    pub updated_by: Option<String>,
    pub recipients: Vec<String>,
}

/// Decrypted group payload. Lives only in memory and inside the age file.
#[derive(Serialize, Deserialize)]
struct GroupData {
    group: String,
    recipients: Vec<String>,
    secrets: HashMap<String, SecretEntry>,
}

#[derive(Serialize, Deserialize)]
struct SecretEntry {
    name: String,
    ciphertext_or_value: String,
    created_at: String,
    updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_by: Option<String>,
}

impl Drop for SecretEntry {
    fn drop(&mut self) {
        self.ciphertext_or_value.zeroize();
    }
}

/// Plaintext sidecar: same shape as [`GroupData`] minus the values.
#[derive(Serialize, Deserialize)]
struct GroupMeta {
    group: String,
    recipients: Vec<String>,
    secrets: Vec<EntryMeta>,
}

#[derive(Serialize, Deserialize)]
struct EntryMeta {
    name: String,
    created_at: String,
    updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_by: Option<String>,
}

/// One integrity problem found by [`Vault::verify`]. Never carries values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub group: String,
    /// The offending file.
    pub path: PathBuf,
    pub message: String,
}

pub struct Vault {
    dir: PathBuf,
    actor: Option<String>,
    policy: Option<VaultPolicy>,
}

/// What `muster revoke` needs to know about a group, from its decrypted
/// payload only (the plaintext sidecar is untrusted).
pub struct GroupView {
    /// Recipients the ciphertext is really encrypted for.
    pub readers: Vec<String>,
    /// Secret names held in the group.
    pub secrets: Vec<String>,
}

const NOT_A_RECIPIENT: &str = "this machine is not a recipient of the secret group";

/// Whether `err` means the identity is simply not a reader of the group.
pub fn is_not_a_recipient(err: &MusterError) -> bool {
    matches!(err, MusterError::Security(m) if m == NOT_A_RECIPIENT)
}

impl Vault {
    /// Opens (creating if needed) `<config_dir>/vault`.
    pub fn open(config_dir: &Path) -> Result<Self, MusterError> {
        let dir = config_dir.join(VAULT_DIR);
        fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        let policy = Manifest::vault_policy(config_dir)?;
        Ok(Self {
            dir,
            actor: None,
            policy,
        })
    }

    /// Also refuse to encrypt to `recipients` (revoked in this operation but
    /// not yet persisted in the manifest).
    pub fn with_revoked(mut self, recipients: impl IntoIterator<Item = String>) -> Self {
        self.policy
            .get_or_insert_with(VaultPolicy::default)
            .revoked
            .extend(recipients);
        self
    }

    /// Records `actor` (a machine name) as author of later adds and rotations.
    pub fn with_actor(mut self, actor: Option<String>) -> Self {
        self.actor = actor;
        self
    }

    /// Adds a new secret. A new group is created for `recipients`; an existing
    /// group keeps its recipients (change them with [`Vault::reencrypt`]).
    pub fn add(
        &mut self,
        group: &str,
        name: &str,
        value: SecretValue,
        recipients: &[String],
        id: &Identity,
    ) -> Result<(), MusterError> {
        validate_segment("group", group)?;
        validate_segment("secret name", name)?;
        let mut data = if self.age_path(group).exists() || self.meta_path(group).exists() {
            self.ensure_pair(group)?;
            let data = self.read_verified_readers(group, id)?;
            self.verify_recipients(group, &data.recipients)?;
            data
        } else {
            self.verify_recipients(group, recipients)?;
            GroupData {
                group: group.to_owned(),
                recipients: recipients.to_vec(),
                secrets: HashMap::new(),
            }
        };
        if data.secrets.contains_key(name) {
            return Err(MusterError::Usage(format!(
                "secret '{group}/{name}' already exists; rotate it instead"
            )));
        }
        let now = now();
        data.secrets.insert(
            name.to_owned(),
            SecretEntry {
                name: name.to_owned(),
                ciphertext_or_value: value.expose().to_owned(),
                created_at: now.clone(),
                updated_at: now,
                created_by: self.actor.clone(),
                updated_by: self.actor.clone(),
            },
        );
        self.write(group, &data)
    }

    pub fn get(&self, group: &str, name: &str, id: &Identity) -> Result<SecretValue, MusterError> {
        let data = self.read(group, id)?;
        let entry = data
            .secrets
            .get(name)
            .ok_or_else(|| not_found(group, name))?;
        Ok(SecretValue::new(entry.ciphertext_or_value.clone()))
    }

    pub fn rotate(
        &mut self,
        group: &str,
        name: &str,
        value: SecretValue,
        id: &Identity,
    ) -> Result<(), MusterError> {
        let mut data = self.read_verified_readers(group, id)?;
        self.verify_recipients(group, &data.recipients)?;
        let entry = data
            .secrets
            .get_mut(name)
            .ok_or_else(|| not_found(group, name))?;
        entry.ciphertext_or_value.zeroize();
        entry.ciphertext_or_value = value.expose().to_owned();
        entry.updated_at = now();
        entry.updated_by = self.actor.clone();
        self.write(group, &data)
    }

    pub fn remove(&mut self, group: &str, name: &str, id: &Identity) -> Result<(), MusterError> {
        let mut data = self.read_verified_readers(group, id)?;
        data.secrets
            .remove(name)
            .ok_or_else(|| not_found(group, name))?;
        self.write(group, &data)
    }

    /// Verifies that both `<group>.age` and `<group>.meta.json` exist. Fails
    /// closed with a Security error if either is missing, or an Operational
    /// error if neither exists.
    pub fn ensure_pair(&self, group: &str) -> Result<(), MusterError> {
        validate_segment("group", group)?;
        let age = self.age_path(group);
        let meta = self.meta_path(group);
        match (age.exists(), meta.exists()) {
            (true, true) => Ok(()),
            (false, false) => Err(MusterError::Operational(format!(
                "secret group '{group}' does not exist"
            ))),
            (true, false) => Err(MusterError::Security(format!(
                "vault group '{group}' is incomplete: missing metadata sidecar '{group}.meta.json'"
            ))),
            (false, true) => Err(MusterError::Security(format!(
                "vault group '{group}' is incomplete: missing ciphertext '{group}.age'"
            ))),
        }
    }

    /// Authentic readers and secret names of `group` decrypted with `id`.
    /// The plaintext sidecar is verified to exist but not trusted for readers.
    pub fn group_view(&self, group: &str, id: &Identity) -> Result<GroupView, MusterError> {
        self.ensure_pair(group)?;
        let data = self.read(group, id)?;
        let mut secrets: Vec<String> = data.secrets.keys().cloned().collect();
        secrets.sort();
        Ok(GroupView {
            readers: data.recipients,
            secrets,
        })
    }

    /// Verifies that `recipients` contains no revoked key, and, when the
    /// manifest defines fleet recipients, only known authorized keys.
    fn verify_recipients(&self, group: &str, recipients: &[String]) -> Result<(), MusterError> {
        let Some(policy) = &self.policy else {
            return Ok(());
        };
        for r in recipients {
            if policy.revoked.contains(r) {
                return Err(MusterError::Security(format!(
                    "recipient '{r}' has been revoked"
                )));
            }
            if !policy.authorized.is_empty() && !policy.authorized.contains(r) {
                return Err(MusterError::Security(format!(
                    "recipient '{r}' is not authorized for group '{group}'"
                )));
            }
        }
        Ok(())
    }

    /// Metadata for every secret in `group`, sorted by name. Needs no identity.
    pub fn list(&self, group: &str) -> Result<Vec<SecretMeta>, MusterError> {
        let meta = self.read_meta(group)?;
        let mut list: Vec<SecretMeta> = meta
            .secrets
            .into_iter()
            .map(|s| SecretMeta {
                name: s.name,
                created_at: s.created_at,
                updated_at: s.updated_at,
                created_by: s.created_by,
                updated_by: s.updated_by,
                recipients: meta.recipients.clone(),
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(list)
    }

    /// Names of every group in the vault, sorted. Enumerates the union of
    /// `<group>.age` and `<group>.meta.json` so incomplete pairs are visible.
    pub fn groups(&self) -> Result<Vec<String>, MusterError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&self.dir, e)),
        };
        let mut groups = BTreeSet::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&self.dir, e))?;
            let file = entry.file_name();
            let file = file.to_string_lossy();
            if let Some(group) = file
                .strip_suffix(".meta.json")
                .or_else(|| file.strip_suffix(".age"))
            {
                groups.insert(group.to_owned());
            }
        }
        Ok(groups.into_iter().collect())
    }

    /// Current recipients (readers) of `group`, from metadata. Needs no identity.
    pub fn readers(&self, group: &str) -> Result<Vec<String>, MusterError> {
        Ok(self.read_meta(group)?.recipients)
    }

    /// Re-encrypts `group` for exactly `new_recipients`.
    pub fn reencrypt(
        &mut self,
        group: &str,
        new_recipients: &[String],
        id: &Identity,
    ) -> Result<(), MusterError> {
        let mut data = self.read_verified_readers(group, id)?;
        data.recipients = new_recipients.to_vec();
        self.write(group, &data)
    }

    fn read_verified_readers(&self, group: &str, id: &Identity) -> Result<GroupData, MusterError> {
        self.ensure_pair(group)?;
        let data = self.read(group, id)?;
        let meta = self.read_meta(group)?;
        if meta.group != group
            || data.group != group
            || meta.recipients.iter().collect::<BTreeSet<_>>() != data.recipients.iter().collect()
        {
            return Err(MusterError::Security(format!(
                "vault metadata reader mismatch for group '{group}'"
            )));
        }
        Ok(data)
    }

    pub(crate) fn verify_readers(&self, group: &str, id: &Identity) -> Result<(), MusterError> {
        self.read_verified_readers(group, id).map(|_| ())
    }

    /// Resolves `native://<group>/<name>` from local files only.
    pub fn resolve(&self, reference: &str, id: &Identity) -> Result<SecretValue, MusterError> {
        let invalid = || {
            MusterError::Usage(format!(
                "invalid secret reference '{reference}': expected native://<group>/<name>"
            ))
        };
        let (group, name) = reference
            .strip_prefix(NATIVE_SCHEME)
            .and_then(|rest| rest.split_once('/'))
            .ok_or_else(invalid)?;
        if validate_segment("group", group).is_err()
            || validate_segment("secret name", name).is_err()
        {
            return Err(invalid());
        }
        self.get(group, name, id)
    }

    /// Integrity of every group: both files present, metadata readable, a
    /// valid age header and, when `id` is one of the group's readers, a
    /// successful decryption (values are dropped at once).
    pub fn verify(&self, id: Option<&Identity>) -> Result<Vec<Problem>, MusterError> {
        let entries = fs::read_dir(&self.dir).map_err(|e| io_err(&self.dir, e))?;
        let mut groups = BTreeSet::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&self.dir, e))?;
            let file = entry.file_name();
            let file = file.to_string_lossy();
            if let Some(group) = file
                .strip_suffix(".meta.json")
                .or_else(|| file.strip_suffix(".age"))
            {
                groups.insert(group.to_owned());
            }
        }
        let mut problems = Vec::new();
        for group in groups {
            let (age_path, meta_path) = (self.age_path(&group), self.meta_path(&group));
            let mut problem = |path: &Path, message: String| {
                problems.push(Problem {
                    group: group.clone(),
                    path: path.to_path_buf(),
                    message,
                })
            };
            if !age_path.is_file() {
                problem(
                    &meta_path,
                    format!("{group}.age is missing (metadata without ciphertext)"),
                );
                continue;
            }
            if !meta_path.is_file() {
                problem(
                    &age_path,
                    format!("{group}.meta.json is missing (ciphertext without metadata)"),
                );
                continue;
            }
            let readers = match self.read_meta(&group) {
                Ok(meta) => meta.recipients,
                Err(e) => {
                    problem(&meta_path, e.to_string());
                    continue;
                }
            };
            let armored = match fs::read(&age_path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    problem(&age_path, io_err(&age_path, e).to_string());
                    continue;
                }
            };
            if let Err(e) = age::Decryptor::new(age::armor::ArmoredReader::new(&armored[..])) {
                problem(&age_path, format!("not a valid age file: {e}"));
                continue;
            }
            if let Some(id) = id.filter(|id| readers.contains(&id.recipient())) {
                if let Err(e) = self.read(&group, id) {
                    problem(
                        &age_path,
                        format!("this machine is a reader but cannot decrypt it: {e}"),
                    );
                }
            }
        }
        Ok(problems)
    }

    fn age_path(&self, group: &str) -> PathBuf {
        self.dir.join(format!("{group}.age"))
    }

    fn meta_path(&self, group: &str) -> PathBuf {
        self.dir.join(format!("{group}.meta.json"))
    }

    fn read_meta(&self, group: &str) -> Result<GroupMeta, MusterError> {
        validate_segment("group", group)?;
        let path = self.meta_path(group);
        let text = fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
        serde_json::from_str(&text).map_err(|e| MusterError::InvalidConfig {
            message: format!("corrupt vault metadata: {e}"),
            path: Some(path.display().to_string()),
        })
    }

    fn read(&self, group: &str, id: &Identity) -> Result<GroupData, MusterError> {
        validate_segment("group", group)?;
        let path = self.age_path(group);
        if !path.exists() {
            return Err(MusterError::Operational(format!(
                "secret group '{group}' does not exist"
            )));
        }
        let armored = fs::read(&path).map_err(|e| io_err(&path, e))?;
        let plaintext = decrypt(&armored, id)?;
        let data: GroupData =
            serde_json::from_slice(&plaintext).map_err(|e| MusterError::InvalidConfig {
                message: format!("corrupt vault group: {e}"),
                path: Some(path.display().to_string()),
            })?;
        // The ciphertext is untrusted input: bind it to the file it came from.
        if data.group != group {
            return Err(MusterError::Security(format!(
                "vault group binding mismatch: '{group}.age' holds another group's data"
            )));
        }
        for (key, entry) in &data.secrets {
            if *key != entry.name || validate_segment("secret name", key).is_err() {
                return Err(MusterError::Security(format!(
                    "vault group '{group}' holds a secret whose name does not match its key"
                )));
            }
        }
        Ok(data)
    }

    /// Paths always derive from the caller's validated `group`, never from
    /// the decrypted payload.
    fn write(&self, group: &str, data: &GroupData) -> Result<(), MusterError> {
        validate_segment("group", group)?;
        if data.group != group {
            return Err(MusterError::Security(format!(
                "refusing to write group data for '{}' into '{group}'",
                data.group
            )));
        }
        if let Some(policy) = &self.policy {
            for r in &data.recipients {
                if policy.revoked.contains(r) {
                    return Err(MusterError::Security(format!(
                        "refusing to encrypt group '{group}' to revoked recipient '{r}'"
                    )));
                }
            }
        }
        let plaintext = Zeroizing::new(
            serde_json::to_vec(data)
                .map_err(|e| MusterError::Operational(format!("cannot serialize group: {e}")))?,
        );
        write_atomic(
            &self.age_path(group),
            &encrypt(&plaintext, &data.recipients)?,
        )?;
        let meta = GroupMeta {
            group: data.group.clone(),
            recipients: data.recipients.clone(),
            secrets: data
                .secrets
                .values()
                .map(|s| EntryMeta {
                    name: s.name.clone(),
                    created_at: s.created_at.clone(),
                    updated_at: s.updated_at.clone(),
                    created_by: s.created_by.clone(),
                    updated_by: s.updated_by.clone(),
                })
                .collect(),
        };
        let json = serde_json::to_vec_pretty(&meta)
            .map_err(|e| MusterError::Operational(format!("cannot serialize metadata: {e}")))?;
        write_atomic(&self.meta_path(group), &json)
    }
}

fn encrypt(plaintext: &[u8], recipients: &[String]) -> Result<Vec<u8>, MusterError> {
    if recipients.is_empty() {
        return Err(MusterError::Usage(
            "a secret group needs at least one recipient".into(),
        ));
    }
    let parsed = recipients
        .iter()
        .map(|r| {
            r.parse::<age::x25519::Recipient>()
                .map_err(|e| MusterError::Usage(format!("invalid age recipient '{r}': {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let encryptor =
        age::Encryptor::with_recipients(parsed.iter().map(|r| r as &dyn age::Recipient))
            .map_err(crypto_err)?;

    let mut out = Vec::new();
    let armor = age::armor::ArmoredWriter::wrap_output(&mut out, age::armor::Format::AsciiArmor)
        .map_err(crypto_err)?;
    let mut writer = encryptor.wrap_output(armor).map_err(crypto_err)?;
    writer.write_all(plaintext).map_err(crypto_err)?;
    writer
        .finish()
        .and_then(|armor| armor.finish())
        .map_err(crypto_err)?;
    Ok(out)
}

fn decrypt(armored: &[u8], id: &Identity) -> Result<Zeroizing<Vec<u8>>, MusterError> {
    let decryptor =
        age::Decryptor::new(age::armor::ArmoredReader::new(armored)).map_err(crypto_err)?;
    let mut reader = decryptor
        .decrypt(std::iter::once(id.as_age() as &dyn age::Identity))
        .map_err(|e| match e {
            age::DecryptError::NoMatchingKeys => MusterError::Security(NOT_A_RECIPIENT.into()),
            other => crypto_err(other),
        })?;
    let mut plaintext = Zeroizing::new(Vec::new());
    reader.read_to_end(&mut plaintext).map_err(crypto_err)?;
    Ok(plaintext)
}

/// Group and secret names become file names and reference segments.
fn validate_segment(kind: &str, value: &str) -> Result<(), MusterError> {
    let ok = !value.is_empty()
        && !value.starts_with('.')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(MusterError::Usage(format!(
            "invalid {kind} '{value}': use letters, digits, '_', '-' or '.'"
        )))
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), MusterError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(tmp);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(|e| io_err(path, e))?;
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| io_err(path, e))
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC 3339 formatting of the current UTC time cannot fail")
}

fn not_found(group: &str, name: &str) -> MusterError {
    MusterError::Operational(format!("secret '{group}/{name}' does not exist"))
}

fn crypto_err(e: impl std::fmt::Display) -> MusterError {
    MusterError::Security(format!("vault crypto error: {e}"))
}

fn io_err(path: &Path, e: std::io::Error) -> MusterError {
    MusterError::Operational(format!("{}: {e}", path.display()))
}
