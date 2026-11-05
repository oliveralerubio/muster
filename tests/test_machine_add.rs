//! `muster machine add`: SSH enrollment, driven end to end with fake `ssh` and
//! `scp` scripts (selected through `MUSTER_SSH` / `MUSTER_SCP`). The fake ssh
//! runs the remote command locally as a second, isolated machine.
mod support;

use assert_cmd::Command;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as Std;
use support::MusterEnv;
use tempfile::TempDir;

const FAKE_SSH: &str = r#"#!/bin/sh
echo "ssh $*" >> "$FAKE_LOG"
shift 3
host=$1
cmd=$2
[ "$host" = unreachable ] && { echo "Connection refused" >&2; exit 255; }
case "$cmd" in
  "uname -s; uname -m; hostname") uname -s; uname -m; echo fakehost ;;
  *) exec env -i HOME="$R_HOME" XDG_CONFIG_HOME="$R_HOME/.config" \
       XDG_STATE_HOME="$R_HOME/.local/state" XDG_DATA_HOME="$R_HOME/.local/share" \
       XDG_CACHE_HOME="$R_HOME/.cache" PATH="$R_BIN:/usr/bin:/bin" sh -c "$cmd" ;;
esac
"#;

const FAKE_SCP: &str = r#"#!/bin/sh
echo "scp $*" >> "$FAKE_LOG"
shift 4
mkdir -p "$R_HOME/.local/bin"
cp "$1" "$R_HOME/${2#*:}"
"#;

struct Fleet {
    _remote_guard: TempDir,
    _tools: TempDir,
    remote: PathBuf,
    log: PathBuf,
    ssh: PathBuf,
    scp: PathBuf,
    alice: MusterEnv,
    bob: MusterEnv,
}

fn script(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn ok(env: &MusterEnv, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (out, envelope) = env.run_json(&full);
    assert!(out.status.success(), "{args:?}: {}", envelope.raw);
    envelope.result().clone()
}

impl Fleet {
    /// alice: initialised, with a bare remote. bob: a blank second machine.
    fn new(with_remote: bool) -> Self {
        let remote_guard = TempDir::new().unwrap();
        let remote = remote_guard.path().join("central.git");
        assert!(Std::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&remote)
            .status()
            .unwrap()
            .success());
        let tools = TempDir::new().unwrap();
        let ssh = script(tools.path(), "ssh", FAKE_SSH);
        let scp = script(tools.path(), "scp", FAKE_SCP);
        let log = tools.path().join("calls.log");
        let alice = MusterEnv::new();
        ok(&alice, &["init", "--yes", "--machine", "alice"]);
        if with_remote {
            ok(&alice, &["sync", "--remote", remote.to_str().unwrap()]);
        }
        Self {
            _remote_guard: remote_guard,
            _tools: tools,
            remote,
            log,
            ssh,
            scp,
            alice,
            bob: MusterEnv::new(),
        }
    }

    /// Give bob a `muster` on his PATH (otherwise machine add must copy one).
    fn install_muster_on_bob(&self) {
        std::os::unix::fs::symlink(
            assert_cmd::cargo::cargo_bin("muster"),
            self.bob.bin.join("muster"),
        )
        .unwrap();
    }

    fn add_cmd(&self, args: &[&str]) -> Command {
        let mut full = vec!["--format", "json", "machine", "add"];
        full.extend_from_slice(args);
        let mut cmd = self.alice.run(&full);
        cmd.env("MUSTER_SSH", &self.ssh)
            .env("MUSTER_SCP", &self.scp)
            .env("FAKE_LOG", &self.log)
            .env("R_HOME", self.bob.path())
            .env("R_BIN", &self.bob.bin);
        cmd
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[test]
fn adds_a_remote_machine_end_to_end_with_muster_already_installed() {
    let fleet = Fleet::new(true);
    fleet.install_muster_on_bob();
    let (out, envelope) =
        support::json(fleet.add_cmd(&["fakehost", "--name", "bob", "--role", "dev", "--yes"]));
    assert!(out.status.success(), "{}", envelope.raw);
    let result = envelope.result();
    assert_eq!(result["machine"], "bob", "{result}");
    assert_eq!(result["host"], "fakehost");
    assert!(result["recipient"].as_str().unwrap().starts_with("age1"));
    assert_eq!(result["platform"].as_str().unwrap(), {
        format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
    });
    assert!(result["steps"].as_array().unwrap().len() >= 5, "{result}");
    assert!(result["applied"].is_boolean());
    assert!(!fleet.calls().contains("scp "), "{}", fleet.calls());
    // The new host must be told which signer to trust (no blind TOFU).
    let fingerprint = ok(&fleet.alice, &["trust", "show"])["fleet_state"]["signer_fingerprint"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        fleet
            .calls()
            .contains(&format!("--expect-signer '{fingerprint}'")),
        "{}",
        fleet.calls()
    );

    // Alice's manifest lists bob (with the role) and the request is gone.
    let machines = ok(&fleet.alice, &["machine", "list"]).to_string();
    assert!(machines.contains("bob"), "{machines}");
    let manifest = std::fs::read_to_string(fleet.alice.config_dir().join("muster.toml")).unwrap();
    assert!(manifest.contains("[machines.bob]") && manifest.contains("dev"));
    assert!(!fleet.alice.config_dir().join("enroll/bob.toml").exists());

    // The remote joined the repo and sees itself as a member; the approval is on the remote.
    let bob_manifest = std::fs::read_to_string(fleet.bob.config_dir().join("muster.toml")).unwrap();
    assert!(bob_manifest.contains("[machines.bob]"), "{bob_manifest}");
    let origin = Std::new("git")
        .args(["-C", fleet.remote.to_str().unwrap(), "log", "--oneline"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&origin.stdout).contains("approve machine bob"));
}

#[test]
fn copies_this_binary_when_the_remote_has_no_muster() {
    let fleet = Fleet::new(true);
    let (out, envelope) = support::json(fleet.add_cmd(&["fakehost", "--name", "bob", "--yes"]));
    assert!(out.status.success(), "{}", envelope.raw);
    assert!(fleet.calls().contains("scp "), "{}", fleet.calls());
    let installed = fleet.bob.path().join(".local/bin/muster");
    assert!(installed.is_file());
    assert_eq!(
        std::fs::metadata(installed).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(envelope.result()["machine"], "bob");
    assert!(
        std::fs::read_to_string(fleet.alice.config_dir().join("muster.toml"))
            .unwrap()
            .contains("[machines.bob]")
    );
}

#[test]
fn defaults_the_name_to_the_remote_hostname() {
    let fleet = Fleet::new(true);
    fleet.install_muster_on_bob();
    let (out, envelope) = support::json(fleet.add_cmd(&["fakehost", "--yes"]));
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result()["machine"], "fakehost");
}

#[test]
fn requires_a_git_remote() {
    let fleet = Fleet::new(false);
    let (out, envelope) = support::json(fleet.add_cmd(&["fakehost", "--name", "bob", "--yes"]));
    assert_eq!(out.status.code(), Some(2), "{}", envelope.raw);
    assert!(envelope.raw.to_string().contains("no git remote"));
    assert!(fleet.calls().is_empty());
}

#[test]
fn rejects_a_name_that_is_already_in_the_fleet() {
    let fleet = Fleet::new(true);
    let (out, envelope) = support::json(fleet.add_cmd(&["fakehost", "--name", "alice", "--yes"]));
    assert_eq!(out.status.code(), Some(2), "{}", envelope.raw);
    assert!(envelope.raw.to_string().contains("already a member"));
    assert!(fleet.calls().is_empty());
}

#[test]
fn rejects_hosts_that_could_inject_options_or_commands() {
    let fleet = Fleet::new(true);
    for host in ["-oProxyCommand=evil", "a b", "host;rm", "$(x)", ""] {
        let (out, _) = support::json(fleet.add_cmd(&["--yes", "--", host]));
        assert_eq!(out.status.code(), Some(2), "{host:?}");
    }
    assert!(fleet.calls().is_empty());
}

#[test]
fn dry_run_lists_the_steps_and_never_runs_ssh() {
    let fleet = Fleet::new(true);
    let mut dry = fleet.alice.run(&[
        "--format",
        "json",
        "--dry-run",
        "machine",
        "add",
        "fakehost",
        "--name",
        "bob",
    ]);
    dry.env("MUSTER_SSH", &fleet.ssh)
        .env("FAKE_LOG", &fleet.log);
    let (out, envelope) = support::json(dry);
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result()["dry_run"], true);
    assert!(envelope.result()["steps"].as_array().unwrap().len() >= 5);
    assert!(fleet.calls().is_empty());
}

#[test]
fn unreachable_host_fails_with_a_clear_error() {
    let fleet = Fleet::new(true);
    let (out, envelope) = support::json(fleet.add_cmd(&["unreachable", "--name", "bob", "--yes"]));
    assert_eq!(out.status.code(), Some(1), "{}", envelope.raw);
    assert!(envelope
        .raw
        .to_string()
        .contains("without a password prompt"));
    assert!(
        !std::fs::read_to_string(fleet.alice.config_dir().join("muster.toml"))
            .unwrap()
            .contains("[machines.bob]")
    );
}

#[test]
fn a_remote_that_cannot_reach_the_repo_gets_a_credentials_hint() {
    let fleet = Fleet::new(true);
    fleet.install_muster_on_bob();
    // Break the origin after alice has pushed: bob cannot fetch it.
    let moved = fleet.remote.with_extension("gone");
    std::fs::rename(&fleet.remote, &moved).unwrap();
    let (out, envelope) = support::json(fleet.add_cmd(&["fakehost", "--name", "bob", "--yes"]));
    assert_eq!(out.status.code(), Some(1), "{}", envelope.raw);
    assert!(envelope.raw.to_string().contains("git access"));
}

#[test]
fn machine_add_refuses_and_restores_when_pulled_remote_corrupts_fleet_state() {
    let fleet = Fleet::new(true);
    fleet.install_muster_on_bob();

    // Custom ssh script: after enroll succeeds, injects a forged commit onto origin central.git
    // that breaks fleet state verification.
    let inject_script = script(
        fleet._tools.path(),
        "ssh_inject",
        &format!(
            r#"#!/bin/sh
cmd="$5"
case "$cmd" in
  *"enroll"*)
    env -i HOME="$R_HOME" XDG_CONFIG_HOME="$R_HOME/.config"        XDG_STATE_HOME="$R_HOME/.local/state" XDG_DATA_HOME="$R_HOME/.local/share"        XDG_CACHE_HOME="$R_HOME/.cache" PATH="$R_BIN:/usr/bin:/bin" sh -c "$cmd"
    rc=$?
    # Corrupt fleet.state.json directly on central.git
    TMPGIT="$(mktemp -d)"
    git clone --quiet "{central}" "$TMPGIT"
    echo '{{"payload":{{"epoch":999}},"signature":"bad"}}' > "$TMPGIT/fleet.state.json"
    (cd "$TMPGIT" && git add fleet.state.json && git -c user.name=x -c user.email=x@x commit -qm "poison" && git push --quiet)
    rm -rf "$TMPGIT"
    exit $rc
    ;;
  *) exec "{orig_ssh}" "$@" ;;
esac
"#,
            central = fleet.remote.to_str().unwrap(),
            orig_ssh = fleet.ssh.to_str().unwrap()
        ),
    );

    let mut cmd = fleet.add_cmd(&["fakehost", "--name", "bob", "--role", "dev", "--yes"]);
    cmd.env("MUSTER_SSH", &inject_script);
    let (out, envelope) = support::json(cmd);
    assert!(!out.status.success(), "{}", envelope.raw);
    assert!(envelope.raw.to_string().contains("discarded"));

    // Alice never approved bob
    let machines = ok(&fleet.alice, &["machine", "list"]).to_string();
    assert!(!machines.contains("bob"), "{machines}");
}
