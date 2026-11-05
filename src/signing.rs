//! Ed25519 signing keys: authenticate the fleet state and enrollment approvals.
//!
//! Each machine keeps a `signing.key` next to `identity.txt` (state dir 0700,
//! file 0600), created lazily. Public keys are published as `musterpk1<hex>`.
//! The offline recovery key derives a deterministic signing key, so the
//! break-glass path can sign too without storing anything new offline.

use std::fs;
use std::io::Write;
use std::path::Path;

use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::errors::MusterError;
use crate::identity::{io_err, restrict, Identity};

const KEY_FILE: &str = "signing.key";
/// Prefix of a published public key.
pub const PUBLIC_PREFIX: &str = "musterpk1";
const RECOVERY_DOMAIN: &[u8] = b"muster recovery signing v1";

/// A signing key held in memory (zeroized on drop by `ed25519-dalek`).
pub struct Signer(SigningKey);

impl Signer {
    /// This machine's key, or `None` if it has none yet.
    pub fn load(state_dir: &Path) -> Result<Option<Self>, MusterError> {
        let path = state_dir.join(KEY_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => Zeroizing::new(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&path, e)),
        };
        let bytes = Zeroizing::new(from_hex(text.trim()).filter(|b| b.len() == 32).ok_or_else(
            || MusterError::Security(format!("no valid signing key in {}", path.display())),
        )?);
        let mut seed = Zeroizing::new([0u8; 32]);
        seed.copy_from_slice(&bytes);
        Ok(Some(Self(SigningKey::from_bytes(&seed))))
    }

    /// This machine's key, created (0600) on first use: migration needs no user step.
    pub fn load_or_create(state_dir: &Path) -> Result<Self, MusterError> {
        if let Some(signer) = Self::load(state_dir)? {
            return Ok(signer);
        }
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::getrandom(&mut *seed)
            .map_err(|e| MusterError::Operational(format!("no randomness available: {e}")))?;
        let signer = Self(SigningKey::from_bytes(&seed));
        fs::create_dir_all(state_dir).map_err(|e| io_err(state_dir, e))?;
        restrict(state_dir, 0o700)?;
        signer.save(&state_dir.join(KEY_FILE))?;
        Ok(signer)
    }

    fn save(&self, path: &Path) -> Result<(), MusterError> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".tmp-{}", std::process::id()));
        let tmp = std::path::PathBuf::from(tmp);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let hex = Zeroizing::new(to_hex(&self.0.to_bytes()));
        let result = options
            .open(&tmp)
            .and_then(|mut f| {
                f.write_all(hex.as_bytes())?;
                f.write_all(b"\n")?;
                f.sync_all()
            })
            .and_then(|()| fs::rename(&tmp, path));
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(io_err(path, e));
        }
        restrict(path, 0o600)
    }

    /// The signing key derived from the offline recovery identity.
    pub fn from_recovery(identity: &Identity) -> Result<Self, MusterError> {
        let secret = identity.secret_key();
        let hk = hkdf::Hkdf::<Sha256>::new(None, secret.expose().trim().as_bytes());
        let mut okm = Zeroizing::new([0u8; 32]);
        hk.expand(RECOVERY_DOMAIN, &mut *okm).map_err(|e| {
            MusterError::Operational(format!("cannot derive recovery signing key: {e}"))
        })?;
        Ok(Self(SigningKey::from_bytes(&okm)))
    }

    /// `musterpk1<hex>`: the value to publish in the manifest.
    pub fn public(&self) -> String {
        format!(
            "{PUBLIC_PREFIX}{}",
            to_hex(self.0.verifying_key().as_bytes())
        )
    }

    /// Detached signature over `message`, hex encoded.
    pub fn sign(&self, message: &[u8]) -> String {
        to_hex(&self.0.sign(message).to_bytes())
    }
}

/// Parses a published public key.
pub fn parse_public(public: &str) -> Result<VerifyingKey, MusterError> {
    let bad = || MusterError::Security(format!("invalid signing key '{public}'"));
    let hex = public.strip_prefix(PUBLIC_PREFIX).ok_or_else(bad)?;
    let bytes: [u8; 32] = from_hex(hex)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(bad)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| bad())
}

/// Checks the detached hex `signature` of `message` against `public`.
pub fn verify(public: &str, message: &[u8], signature: &str) -> Result<(), MusterError> {
    let key = parse_public(public)?;
    let bad = || MusterError::Security("invalid signature".into());
    let bytes: [u8; 64] = from_hex(signature)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(bad)?;
    key.verify(message, &Signature::from_bytes(&bytes))
        .map_err(|_| bad())
}

/// Short human-checkable fingerprint of a public key: `SHA256:` + 16 hex chars.
pub fn fingerprint(public: &str) -> String {
    let digest = Sha256::digest(public.as_bytes());
    format!("SHA256:{}", to_hex(&digest[..8]))
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_or_create_persists_with_correct_permissions() {
        let dir = tempdir().unwrap();
        let state = dir.path().join("state");
        let s1 = Signer::load_or_create(&state).unwrap();
        let key_file = state.join(KEY_FILE);
        assert!(key_file.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = fs::metadata(&key_file).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            let dirmeta = fs::metadata(&state).unwrap();
            assert_eq!(dirmeta.permissions().mode() & 0o777, 0o700);
        }

        let s2 = Signer::load_or_create(&state).unwrap();
        assert_eq!(s1.public(), s2.public());
        let s3 = Signer::load(&state).unwrap().unwrap();
        assert_eq!(s1.public(), s3.public());
    }

    #[test]
    fn recovery_signing_is_deterministic() {
        let id1 = Identity::generate();
        let s1 = Signer::from_recovery(&id1).unwrap();
        let s2 = Signer::from_recovery(&id1).unwrap();
        assert_eq!(s1.public(), s2.public());

        let id2 = Identity::generate();
        let s3 = Signer::from_recovery(&id2).unwrap();
        assert_ne!(s1.public(), s3.public());
    }

    #[test]
    fn sign_and_verify() {
        let dir = tempdir().unwrap();
        let signer = Signer::load_or_create(dir.path()).unwrap();
        let msg = b"muster test payload 123";
        let sig = signer.sign(msg);
        assert!(verify(&signer.public(), msg, &sig).is_ok());

        // Tampered message
        assert!(verify(&signer.public(), b"tampered", &sig).is_err());

        // Wrong signer
        let other = Signer::from_recovery(&Identity::generate()).unwrap();
        assert!(verify(&other.public(), msg, &sig).is_err());

        // Tampered signature
        let mut bad_sig = sig.clone();
        bad_sig.replace_range(0..2, "00");
        if bad_sig != sig {
            assert!(verify(&signer.public(), msg, &bad_sig).is_err());
        }
    }

    #[test]
    fn parse_public_key() {
        let dir = tempdir().unwrap();
        let signer = Signer::load_or_create(dir.path()).unwrap();
        let pub_str = signer.public();
        assert!(pub_str.starts_with(PUBLIC_PREFIX));
        assert!(parse_public(&pub_str).is_ok());
        assert!(parse_public("invalid").is_err());
        assert!(parse_public(&format!("{PUBLIC_PREFIX}1234")).is_err());
    }

    #[test]
    fn key_fingerprint() {
        let dir = tempdir().unwrap();
        let signer = Signer::load_or_create(dir.path()).unwrap();
        let fp = fingerprint(&signer.public());
        assert!(fp.starts_with("SHA256:"));
        assert_eq!(fp.len(), 7 + 16);
    }
}
