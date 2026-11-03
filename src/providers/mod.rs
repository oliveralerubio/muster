//! Secret providers behind one interface: `describe`, `check`, `resolve`,
//! `close`. `native` (the vault, default) and `env` are built in; `op`, `bws`,
//! `doppler` and `infisical` shell out to their vendor CLI when it is on PATH.
//!
//! Callers go through [`Providers`]: it parses a reference, opens the matching
//! provider on first use and closes every opened provider when dropped.

pub mod cli;
mod env;
mod native;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::errors::MusterError;
use crate::secrets::{Scheme, SecretRef, SecretValue};

/// The typed failures every provider reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// Provider (or its CLI) missing or not working. Exit 4.
    Unavailable,
    /// Not signed in, expired session, or this machine may not read it. Exit 5.
    Unauthorized,
    /// The reference points at nothing. Exit 1.
    NotFound,
    /// The provider throttled us. Exit 4.
    RateLimited,
    /// Malformed reference or unknown scheme. Exit 2.
    InvalidReference,
}

/// A provider failure. Messages never contain secret values.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Stable diagnostic code.
    pub fn code(&self) -> &'static str {
        match self.kind {
            ProviderErrorKind::Unavailable => "provider_unavailable",
            ProviderErrorKind::Unauthorized => "provider_unauthorized",
            ProviderErrorKind::NotFound => "secret_not_found",
            ProviderErrorKind::RateLimited => "provider_rate_limited",
            ProviderErrorKind::InvalidReference => "invalid_secret_reference",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self.kind {
            ProviderErrorKind::Unavailable | ProviderErrorKind::RateLimited => 4,
            ProviderErrorKind::Unauthorized => 5,
            ProviderErrorKind::NotFound => 1,
            ProviderErrorKind::InvalidReference => 2,
        }
    }
}

/// What a provider is, for `secret check` and the UIs.
#[derive(Debug, Clone, Serialize)]
pub struct Description {
    pub scheme: Scheme,
    pub name: &'static str,
    /// Built in (`native`, `env`) or backed by an optional vendor CLI.
    pub builtin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary: Option<&'static str>,
}

/// The common provider interface.
pub trait Provider {
    fn describe(&self) -> Description;
    /// Ready to resolve (CLI installed, identity present...). Does not read secrets.
    fn check(&mut self) -> Result<(), MusterError>;
    /// Verify `reference` without materialising its plaintext. `Ok(true)`: the
    /// secret is known to exist and be readable here; `Ok(false)`: only the
    /// provider was checked (vendor CLIs cannot tell without reading the value).
    fn locate(&mut self, _reference: &SecretRef) -> Result<bool, MusterError> {
        self.check()?;
        Ok(false)
    }
    /// The plaintext for `reference`; its scheme always matches this provider.
    fn resolve(&mut self, reference: &SecretRef) -> Result<SecretValue, MusterError>;
    /// Drop cached keys or sessions. Called once, when [`Providers`] is dropped.
    fn close(&mut self) {}
}

/// Lazily opened providers for one command run.
pub struct Providers {
    config_dir: PathBuf,
    state_dir: PathBuf,
    open: BTreeMap<Scheme, Box<dyn Provider>>,
}

impl Providers {
    pub fn new(config_dir: &Path, state_dir: &Path) -> Self {
        Self {
            config_dir: config_dir.to_owned(),
            state_dir: state_dir.to_owned(),
            open: BTreeMap::new(),
        }
    }

    /// The provider for `scheme`, opened on first use.
    pub fn provider(&mut self, scheme: Scheme) -> &mut dyn Provider {
        let (config_dir, state_dir) = (&self.config_dir, &self.state_dir);
        self.open
            .entry(scheme)
            .or_insert_with(|| build(scheme, config_dir, state_dir))
            .as_mut()
    }

    /// Parse `reference` and resolve it through its provider.
    pub fn resolve(&mut self, reference: &str) -> Result<SecretValue, MusterError> {
        let reference = SecretRef::parse(reference)?;
        self.resolve_ref(&reference)
    }

    pub fn resolve_ref(&mut self, reference: &SecretRef) -> Result<SecretValue, MusterError> {
        self.provider(reference.scheme()).resolve(reference)
    }

    /// See [`Provider::locate`].
    pub fn locate(&mut self, reference: &SecretRef) -> Result<bool, MusterError> {
        self.provider(reference.scheme()).locate(reference)
    }
}

impl Drop for Providers {
    fn drop(&mut self) {
        for provider in self.open.values_mut() {
            provider.close();
        }
    }
}

fn build(scheme: Scheme, config_dir: &Path, state_dir: &Path) -> Box<dyn Provider> {
    match scheme {
        Scheme::Native => Box::new(native::Native::new(config_dir, state_dir)),
        Scheme::Env => Box::new(env::Env),
        Scheme::Op => Box::new(cli::Cli::new(&cli::OP)),
        Scheme::Bws => Box::new(cli::Cli::new(&cli::BWS)),
        Scheme::Doppler => Box::new(cli::Cli::new(&cli::DOPPLER)),
        Scheme::Infisical => Box::new(cli::Cli::new(&cli::INFISICAL)),
    }
}
