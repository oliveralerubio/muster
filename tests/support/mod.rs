#![allow(dead_code)] // each test crate uses a different subset of the helpers

use assert_cmd::Command;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Output;
use tempfile::TempDir;

/// Isolated, simulated machine: fake HOME, XDG dirs, PATH with a bin dir.
pub struct MusterEnv {
    pub home: TempDir,
    pub bin: PathBuf,
}

impl MusterEnv {
    pub fn new() -> Self {
        let home = TempDir::new().expect("temp home");
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        for sub in [".config", ".local/state", ".local/share", ".cache"] {
            std::fs::create_dir_all(home.path().join(sub)).expect("xdg dir");
        }
        Self { home, bin }
    }

    pub fn path(&self) -> PathBuf {
        self.home.path().to_path_buf()
    }

    /// Default config repo path: `$XDG_CONFIG_HOME/muster`.
    pub fn config_dir(&self) -> PathBuf {
        self.home.path().join(".config").join("muster")
    }

    /// `muster <args>` with isolated HOME/XDG, temp bin dir first on PATH,
    /// no inherited `MUSTER_*` variables and an empty, non-TTY stdin.
    pub fn run(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("muster").expect("muster binary");
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        cmd.args(args)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_STATE_HOME", self.home.path().join(".local/state"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            .env("PATH", path)
            .write_stdin(b"");
        for (k, _) in std::env::vars() {
            if k.starts_with("MUSTER_") {
                cmd.env_remove(&k);
            }
        }
        cmd
    }

    /// Run and parse stdout as a JSON envelope.
    pub fn run_json(&self, args: &[&str]) -> (Output, Envelope) {
        json(self.run(args))
    }
}

/// Run a prepared command and parse its stdout as exactly one JSON envelope.
pub fn json(mut cmd: Command) -> (Output, Envelope) {
    let output = cmd.output().expect("run muster");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let raw: Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON {e}: {stdout}"));
    (output, Envelope { raw })
}

pub struct Envelope {
    pub raw: Value,
}

impl Envelope {
    pub fn schema_version(&self) -> i64 {
        self.raw["schema_version"]
            .as_i64()
            .expect("schema_version must be an integer")
    }
    pub fn command(&self) -> &str {
        self.raw["command"]
            .as_str()
            .expect("command must be a string")
    }
    pub fn ok(&self) -> bool {
        self.raw["ok"].as_bool().expect("ok must be a bool")
    }
    pub fn result(&self) -> &Value {
        &self.raw["result"]
    }
    pub fn diagnostics(&self) -> &Value {
        &self.raw["diagnostics"]
    }
}

impl MusterEnv {
    /// Put an executable `#!/bin/sh` script named `name` in the temp bin dir.
    pub fn fake_exe(&self, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.bin.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("write fake exe");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

impl MusterEnv {
    /// Like [`MusterEnv::run`], but PATH is ONLY the temp bin dir, so the
    /// host's real agent CLIs never leak in. Link what the test needs with
    /// [`MusterEnv::link_system_exe`] (e.g. `git`).
    pub fn run_isolated(&self, args: &[&str]) -> Command {
        let mut cmd = self.run(args);
        cmd.env("PATH", &self.bin);
        cmd
    }

    /// Symlink the host's `name` executable (found on the real PATH) into the
    /// temp bin dir.
    pub fn link_system_exe(&self, name: &str) {
        let link = self.bin.join(name);
        if link.exists() {
            return;
        }
        let path = std::env::var_os("PATH").expect("PATH");
        let target = std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("`{name}` not found on PATH"));
        std::os::unix::fs::symlink(target, link).expect("symlink system exe");
    }
}

/// The public key that signed the fleet state in `config_dir`: what a new
/// machine passes as `--expect-signer` on first use.
pub fn fleet_signer(config_dir: &std::path::Path) -> String {
    let text = std::fs::read_to_string(config_dir.join("fleet.state.json")).expect("fleet state");
    let state: Value = serde_json::from_str(&text).expect("fleet state json");
    state["payload"]["signer"]
        .as_str()
        .expect("signer")
        .to_string()
}
