//! Machine identity: an age x25519 key kept only in the local state dir.

use std::fs;
use std::io::Write;
use std::path::Path;

use secrecy::ExposeSecret;

use crate::errors::MusterError;
use crate::secrets::SecretValue;

const IDENTITY_FILE: &str = "identity.txt";

/// This machine's age x25519 identity.
pub struct Identity(age::x25519::Identity);

impl Identity {
    pub fn generate() -> Self {
        Self(age::x25519::Identity::generate())
    }

    /// The public `age1...` recipient string.
    pub fn recipient(&self) -> String {
        self.0.to_public().to_string()
    }

    /// Writes `<state_dir>/identity.txt` (dir 0700, file 0600 on Unix).
    pub fn save(&self, state_dir: &Path) -> Result<(), MusterError> {
        fs::create_dir_all(state_dir).map_err(|e| io_err(state_dir, e))?;
        restrict(state_dir, 0o700)?;
        let path = state_dir.join(IDENTITY_FILE);
        let mut file = open_private(&path)?;
        restrict(&path, 0o600)?;
        let key = self.0.to_string();
        writeln!(
            file,
            "# muster machine identity\n# public key: {}",
            self.recipient()
        )
        .and_then(|()| writeln!(file, "{}", key.expose_secret()))
        .map_err(|e| io_err(&path, e))
    }

    /// Reads `<state_dir>/identity.txt`.
    pub fn load(state_dir: &Path) -> Result<Self, MusterError> {
        let path = state_dir.join(IDENTITY_FILE);
        let text = fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
        text.lines()
            .map(str::trim)
            .find(|l| l.starts_with("AGE-SECRET-KEY-"))
            .and_then(|l| l.parse::<age::x25519::Identity>().ok())
            .map(Self)
            .ok_or_else(|| {
                MusterError::Security(format!("no valid age identity in {}", path.display()))
            })
    }

    /// Parses an `AGE-SECRET-KEY-...` string (e.g. the offline recovery key).
    pub fn from_secret(key: &SecretValue) -> Result<Self, MusterError> {
        key.expose()
            .trim()
            .parse::<age::x25519::Identity>()
            .map(Self)
            .map_err(|_| MusterError::Security("invalid age secret key".into()))
    }

    /// The `AGE-SECRET-KEY-...` string, wrapped so it cannot be printed by accident.
    pub fn secret_key(&self) -> SecretValue {
        SecretValue::new(self.0.to_string().expose_secret().to_owned())
    }

    pub(crate) fn as_age(&self) -> &age::x25519::Identity {
        &self.0
    }
}

#[cfg(unix)]
fn open_private(path: &Path) -> Result<fs::File, MusterError> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| io_err(path, e))
}

#[cfg(not(unix))]
fn open_private(path: &Path) -> Result<fs::File, MusterError> {
    fs::File::create(path).map_err(|e| io_err(path, e))
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<(), MusterError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| {
        MusterError::Security(format!("cannot set permissions on {}: {e}", path.display()))
    })
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<(), MusterError> {
    Ok(())
}

fn io_err(path: &Path, e: std::io::Error) -> MusterError {
    MusterError::Operational(format!("{}: {e}", path.display()))
}
