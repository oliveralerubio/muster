//! Native vault: one age-armored file per secret group, encrypted as a unit
//! for the group's recipients, plus a plaintext metadata sidecar (no values)
//! so `list` works without an identity.
//!
//! Layout under the config repo:
//! - `vault/<group>.age`       armored age ciphertext of [`GroupData`] JSON
//! - `vault/<group>.meta.json` names, timestamps and recipients only

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::MusterError;
use crate::identity::Identity;
use crate::secrets::SecretValue;

const VAULT_DIR: &str = "vault";
const NATIVE_SCHEME: &str = "native://";

/// A secret's metadata. Never carries the value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretMeta {
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
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
}

pub struct Vault {
    dir: PathBuf,
}

impl Vault {
    /// Opens (creating if needed) `<config_dir>/vault`.
    pub fn open(config_dir: &Path) -> Result<Self, MusterError> {
        let dir = config_dir.join(VAULT_DIR);
        fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        Ok(Self { dir })
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
        let mut data = if self.age_path(group).exists() {
            self.read(group, id)?
        } else {
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
            },
        );
        self.write(&data)
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
        let mut data = self.read(group, id)?;
        let entry = data
            .secrets
            .get_mut(name)
            .ok_or_else(|| not_found(group, name))?;
        entry.ciphertext_or_value = value.expose().to_owned();
        entry.updated_at = now();
        self.write(&data)
    }

    pub fn remove(&mut self, group: &str, name: &str, id: &Identity) -> Result<(), MusterError> {
        let mut data = self.read(group, id)?;
        data.secrets
            .remove(name)
            .ok_or_else(|| not_found(group, name))?;
        self.write(&data)
    }

    /// Metadata for every secret in `group`, sorted by name. Needs no identity.
    pub fn list(&self, group: &str) -> Result<Vec<SecretMeta>, MusterError> {
        validate_segment("group", group)?;
        let path = self.meta_path(group);
        let text = fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
        let meta: GroupMeta =
            serde_json::from_str(&text).map_err(|e| MusterError::InvalidConfig {
                message: format!("corrupt vault metadata: {e}"),
                path: Some(path.display().to_string()),
            })?;
        let mut list: Vec<SecretMeta> = meta
            .secrets
            .into_iter()
            .map(|s| SecretMeta {
                name: s.name,
                created_at: s.created_at,
                updated_at: s.updated_at,
                recipients: meta.recipients.clone(),
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(list)
    }

    /// Re-encrypts `group` for exactly `new_recipients`.
    pub fn reencrypt(
        &mut self,
        group: &str,
        new_recipients: &[String],
        id: &Identity,
    ) -> Result<(), MusterError> {
        let mut data = self.read(group, id)?;
        data.recipients = new_recipients.to_vec();
        self.write(&data)
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

    fn age_path(&self, group: &str) -> PathBuf {
        self.dir.join(format!("{group}.age"))
    }

    fn meta_path(&self, group: &str) -> PathBuf {
        self.dir.join(format!("{group}.meta.json"))
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
        serde_json::from_slice(&plaintext).map_err(|e| MusterError::InvalidConfig {
            message: format!("corrupt vault group: {e}"),
            path: Some(path.display().to_string()),
        })
    }

    fn write(&self, data: &GroupData) -> Result<(), MusterError> {
        let plaintext = serde_json::to_vec(data)
            .map_err(|e| MusterError::Operational(format!("cannot serialize group: {e}")))?;
        write_atomic(
            &self.age_path(&data.group),
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
                })
                .collect(),
        };
        let json = serde_json::to_vec_pretty(&meta)
            .map_err(|e| MusterError::Operational(format!("cannot serialize metadata: {e}")))?;
        write_atomic(&self.meta_path(&data.group), &json)
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

fn decrypt(armored: &[u8], id: &Identity) -> Result<Vec<u8>, MusterError> {
    let decryptor =
        age::Decryptor::new(age::armor::ArmoredReader::new(armored)).map_err(crypto_err)?;
    let mut reader = decryptor
        .decrypt(std::iter::once(id.as_age() as &dyn age::Identity))
        .map_err(|e| match e {
            age::DecryptError::NoMatchingKeys => {
                MusterError::Security("this machine is not a recipient of the secret group".into())
            }
            other => crypto_err(other),
        })?;
    let mut plaintext = Vec::new();
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

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), MusterError> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes)
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(|e| io_err(path, e))
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
