//! Execution context handed to every command handler: resolved paths, global
//! flags, TTY detection, confirmation and the diagnostics sink.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use crate::errors::MusterError;
use crate::output::{Diagnostic, Format};
use crate::secrets::SecretValue;
use crate::signing::Signer;

/// Global options after CLI/`MUSTER_*` precedence has been applied.
#[derive(Debug, Clone, Default)]
pub struct Globals {
    pub config: Option<PathBuf>,
    pub format: Option<Format>,
    pub yes: bool,
    pub dry_run: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub audit_actor: Option<String>,
    pub web_approval: bool,
}

pub struct Context {
    /// Config repository (`--config` > `MUSTER_CONFIG` > `$XDG_CONFIG_HOME/muster`).
    pub config_dir: PathBuf,
    /// Local, per-machine state (never inside the config repo).
    pub state_dir: PathBuf,
    pub format: Format,
    pub yes: bool,
    pub dry_run: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub audit_actor: Option<String>,
    pub web_approval: bool,
    pub is_tty: bool,
    /// Break-glass signing key derived from the recovery key, set when the
    /// command received the recovery key (see `team::recovery_identity`).
    pub recovery_signer: Option<Signer>,
    /// `--expect-signer`: full 256-bit fingerprint or full public key the fleet state's first
    /// signer must have when this machine pins it for the first time.
    pub expect_signer: Option<String>,
    /// `--expect-state`: payload SHA-256 of the only state this machine may take as its
    /// origin when it pins a fleet for the first time.
    pub expect_state: Option<String>,
    /// Signed enrollment approvals the next fleet-state seal carries (set by
    /// `approve` when a quorum admitted a machine).
    pub enrollments: Vec<crate::fleet_state::EnrollmentEvidence>,
    /// The repo holds a governance change that is short of its admin
    /// co-signatures: this machine keeps using the last accepted state and
    /// commands that would consume the new one refuse (see `trust::guard`).
    pub governance_pending: bool,
    /// Fleet state is invalid (bad signature, forged vault, tampered governance).
    pub fleet_state_invalid: bool,
    diagnostics: Vec<Diagnostic>,
    failed: bool,
}

impl Context {
    pub fn new(globals: Globals) -> Result<Self, MusterError> {
        let home = env_path("HOME");
        let config_dir = match globals.config {
            Some(dir) => dir,
            None => xdg_dir("XDG_CONFIG_HOME", home.as_deref(), ".config")?,
        };
        Ok(Self {
            config_dir,
            state_dir: state_dir(home.as_deref())?,
            format: globals.format.unwrap_or(Format::Text),
            yes: globals.yes,
            dry_run: globals.dry_run,
            quiet: globals.quiet,
            verbose: globals.verbose,
            audit_actor: globals.audit_actor,
            web_approval: globals.web_approval,
            is_tty: std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
            recovery_signer: None,
            expect_signer: None,
            expect_state: None,
            enrollments: Vec::new(),
            governance_pending: false,
            fleet_state_invalid: false,
            diagnostics: Vec::new(),
            failed: false,
        })
    }

    pub fn diag(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    /// The command ran and has a result, but must report `ok: false` and exit 1
    /// (e.g. `doctor` found error diagnostics).
    pub fn mark_failed(&mut self) {
        self.failed = true;
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Progress line on stderr, silenced by `--quiet`.
    pub fn progress(&self, message: &str) {
        if !self.quiet {
            eprintln!("{message}");
        }
    }

    /// Extra detail on stderr, shown only with `--verbose`.
    pub fn detail(&self, message: &str) {
        if self.verbose {
            eprintln!("{message}");
        }
    }

    /// Ask before a destructive action. `--yes` approves; without a TTY there is
    /// never a prompt, so the action fails with a usage error (exit 2).
    pub fn confirm(&self, question: &str) -> Result<(), MusterError> {
        if self.yes {
            return Ok(());
        }
        if !self.is_tty {
            return Err(MusterError::Usage(format!(
                "{question}: confirmation required but no terminal is attached; re-run with --yes"
            )));
        }
        eprint!("{question} [y/N] ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|e| MusterError::Operational(format!("cannot read answer: {e}")))?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => Ok(()),
            _ => Err(MusterError::Operational("aborted by user".into())),
        }
    }
}

impl Context {
    /// A secret value piped on stdin (one trailing newline dropped). Refuses a
    /// terminal: typed input would echo, and argv would leak to `ps`/history.
    pub fn read_secret_stdin(&self) -> Result<SecretValue, MusterError> {
        use secrecy::zeroize::Zeroize;
        let mut stdin = std::io::stdin().lock();
        if stdin.is_terminal() {
            return Err(MusterError::Usage(
                "pipe the secret value on stdin (e.g. `pbpaste | muster secret add app/api --value-stdin`)".into(),
            ));
        }
        let mut bytes = Vec::new();
        if let Err(e) = stdin.read_to_end(&mut bytes) {
            bytes.zeroize();
            return Err(MusterError::Operational(format!("cannot read stdin: {e}")));
        }
        let mut text = String::from_utf8(bytes).map_err(|e| {
            e.into_bytes().zeroize();
            MusterError::Usage("the secret value must be UTF-8 text".into())
        })?;
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        if text.is_empty() {
            return Err(MusterError::Usage(
                "the secret value on stdin is empty".into(),
            ));
        }
        Ok(SecretValue::new(text))
    }
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `$VAR/muster`, else `$HOME/<fallback>/muster`.
fn xdg_dir(var: &str, home: Option<&Path>, fallback: &str) -> Result<PathBuf, MusterError> {
    match env_path(var) {
        Some(base) => Ok(base.join("muster")),
        None => Ok(require_home(home)?.join(fallback).join("muster")),
    }
}

fn state_dir(home: Option<&Path>) -> Result<PathBuf, MusterError> {
    if cfg!(target_os = "macos") && env_path("XDG_STATE_HOME").is_none() {
        return Ok(require_home(home)?.join("Library/Application Support/muster"));
    }
    xdg_dir("XDG_STATE_HOME", home, ".local/state")
}

fn require_home(home: Option<&Path>) -> Result<&Path, MusterError> {
    home.ok_or_else(|| MusterError::Operational("HOME is not set".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(yes: bool) -> Context {
        Context {
            config_dir: PathBuf::new(),
            state_dir: PathBuf::new(),
            format: Format::Text,
            yes,
            dry_run: false,
            quiet: false,
            verbose: false,
            audit_actor: None,
            web_approval: false,
            is_tty: false,
            recovery_signer: None,
            expect_signer: None,
            expect_state: None,
            enrollments: Vec::new(),
            governance_pending: false,
            fleet_state_invalid: false,
            diagnostics: Vec::new(),
            failed: false,
        }
    }

    #[test]
    fn confirm_without_tty_or_yes_is_a_usage_error() {
        let err = ctx(false).confirm("delete everything").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("--yes"));
    }

    #[test]
    fn confirm_with_yes_never_prompts() {
        assert!(ctx(true).confirm("delete everything").is_ok());
    }
}
