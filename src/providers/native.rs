//! `native://group/name`: Muster's own age vault in the config repo, read
//! with this machine's identity.

use std::path::{Path, PathBuf};

use super::{Description, Provider, ProviderError, ProviderErrorKind};
use crate::errors::MusterError;
use crate::identity::Identity;
use crate::secrets::{Scheme, SecretRef, SecretValue};
use crate::vault::Vault;

pub struct Native {
    config_dir: PathBuf,
    state_dir: PathBuf,
    identity: Option<Identity>,
}

impl Native {
    pub fn new(config_dir: &Path, state_dir: &Path) -> Self {
        Self {
            config_dir: config_dir.to_owned(),
            state_dir: state_dir.to_owned(),
            identity: None,
        }
    }

    fn identity(&mut self) -> Result<&Identity, MusterError> {
        if self.identity.is_none() {
            let identity = Identity::load(&self.state_dir).map_err(|e| {
                ProviderError::new(
                    ProviderErrorKind::Unavailable,
                    format!("native vault: this machine has no usable identity ({e}); run `muster init` or `muster enroll`"),
                )
            })?;
            self.identity = Some(identity);
        }
        Ok(self.identity.as_ref().expect("just loaded"))
    }
}

impl Provider for Native {
    fn describe(&self) -> Description {
        Description {
            scheme: Scheme::Native,
            name: "Muster vault",
            builtin: true,
            binary: None,
        }
    }

    fn check(&mut self) -> Result<(), MusterError> {
        self.identity()?;
        if !self.config_dir.join("vault").is_dir() {
            return Err(ProviderError::new(
                ProviderErrorKind::Unavailable,
                format!(
                    "native vault: {} has no vault; run `muster init`",
                    self.config_dir.display()
                ),
            )
            .into());
        }
        Ok(())
    }

    fn resolve(&mut self, reference: &SecretRef) -> Result<SecretValue, MusterError> {
        let [group, name] = reference.segments() else {
            unreachable!("SecretRef::parse guarantees native://<group>/<name>");
        };
        let not_found = || {
            ProviderError::new(
                ProviderErrorKind::NotFound,
                format!("secret {reference} does not exist in the vault"),
            )
        };
        // Metadata first: tells "missing" apart from "cannot decrypt" without a key.
        let vault = Vault::open(&self.config_dir)?;
        if !vault.groups()?.contains(group) || !vault.list(group)?.iter().any(|m| &m.name == name) {
            return Err(not_found().into());
        }
        let identity = self.identity()?;
        vault.get(group, name, identity).map_err(|e| match e {
            MusterError::Security(message) => ProviderError::new(
                ProviderErrorKind::Unauthorized,
                format!("native vault: cannot read {reference}: {message}"),
            )
            .into(),
            other => other,
        })
    }

    fn close(&mut self) {
        self.identity = None;
    }
}
