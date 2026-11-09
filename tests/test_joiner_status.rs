//! The joining machine's view of `muster status`: the person who ran
//! `muster enroll` must see whether their MACHINE enrollment request is
//! still pending, whether it was published to the fleet remote at all, and
//! the next action — without ever being told they have access before an
//! approval is pulled. (Pending SECRET approvals are a different concern and
//! live in `status` for the approving human, not for the joiner.)

mod support;

use serde_json::Value;
use std::process::Command;
use support::MusterEnv;

fn ok(env: &MusterEnv, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (out, envelope) = env.run_json(&full);
    assert!(out.status.success(), "{args:?}: {}", envelope.raw);
    assert!(envelope.ok(), "{args:?}: {}", envelope.raw);
    envelope.result().clone()
}

fn fails(env: &MusterEnv, args: &[&str], code: i32) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (out, envelope) = env.run_json(&full);
    assert_eq!(out.status.code(), Some(code), "{args:?}: {}", envelope.raw);
    envelope.raw
}

fn text(env: &MusterEnv, args: &[&str]) -> String {
    let out = env.run(args).output().expect("run muster text");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{args:?} text failed: {stdout}");
    stdout
}

fn bare_remote() -> (tempfile::TempDir, String) {
    let dir = tempfile::TempDir::new().expect("remote tempdir");
    let path = dir.path().join("origin.git");
    let status = Command::new("git")
        .args(["init", "--bare", "--", path.to_str().unwrap()])
        .status()
        .expect("git init --bare");
    assert!(status.success());
    (dir, path.to_str().unwrap().to_owned())
}

/// Admin machine `alpha` owns the fleet on `remote`; returns the fleet
/// signer a joining machine must pin on first sync.
fn fleet_admin(remote: &str) -> MusterEnv {
    let admin = MusterEnv::new();
    ok(&admin, &["init", "--yes", "--machine", "alpha"]);
    ok(&admin, &["sync", "--remote", remote, "--push"]);
    admin
}

/// A second machine that has pulled the fleet config but nothing else.
fn joiner(remote: &str, signer: &str) -> MusterEnv {
    let joiner = MusterEnv::new();
    ok(
        &joiner,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            signer,
        ],
    );
    joiner
}

#[test]
fn joining_machine_sees_its_pending_enrollment_in_status() {
    let (_remote_guard, remote) = bare_remote();
    let admin = fleet_admin(&remote);
    let signer = support::fleet_signer(&admin.config_dir());
    let laptop = joiner(&remote, &signer);
    ok(&laptop, &["enroll", "--name", "laptop"]);

    let status = ok(&laptop, &["status"]);
    let enrollment = &status["enrollment"];
    assert_eq!(enrollment["state"], "pending", "{status}");
    assert_eq!(enrollment["machine"], "laptop");
    assert_eq!(enrollment["member"], false);
    assert_eq!(enrollment["published"], true, "{status}");
    assert_eq!(enrollment["approvals_received"], 0);
    assert_eq!(enrollment["approvals_required"], 1);
    assert_eq!(enrollment["invalid_approvals"], 0);
    let next = enrollment["next"].as_array().unwrap();
    assert!(
        next.iter()
            .any(|n| n.as_str().unwrap().contains("muster approve laptop")),
        "next actions must name the approval step: {enrollment}"
    );
    assert!(
        next.iter()
            .any(|n| n.as_str().unwrap().contains("muster sync --pull")),
        "next actions must name the pull step: {enrollment}"
    );
    // No premature membership: this machine is nobody in the machines table.
    assert!(
        !status["machines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["is_current"] == Value::Bool(true)),
        "joiner marked as current machine before approval: {status}"
    );

    // Text mode: the correct status, the honest no-access line and the next
    // actions — not a JSON dump and not a membership claim.
    let stdout = text(&laptop, &["status"]);
    assert!(
        stdout.contains("not a fleet member yet"),
        "missing pending status: {stdout}"
    );
    assert!(
        stdout.contains("no access to fleet secrets"),
        "missing no-access honesty: {stdout}"
    );
    assert!(
        stdout.contains("Published to the fleet remote: yes"),
        "missing publication status: {stdout}"
    );
    assert!(
        stdout.contains("0 of 1 required approvals"),
        "missing approval progress: {stdout}"
    );
    assert!(
        stdout.contains("muster approve laptop"),
        "missing next action: {stdout}"
    );
    assert!(
        stdout.contains("sync --pull"),
        "missing pull next action: {stdout}"
    );
    assert!(
        !stdout.contains("(this machine)"),
        "text claims membership before approval: {stdout}"
    );
    assert!(
        stdout.contains("only as fresh as its last sync"),
        "missing stale-copy caveat: {stdout}"
    );

    // The admin machine is a member: no enrollment section for it.
    let status = ok(&admin, &["status"]);
    assert_eq!(status["enrollment"], Value::Null, "{status}");
}

#[test]
fn enrollment_section_clears_once_the_approval_is_pulled() {
    let (_remote_guard, remote) = bare_remote();
    let admin = fleet_admin(&remote);
    let signer = support::fleet_signer(&admin.config_dir());
    let laptop = joiner(&remote, &signer);
    ok(&laptop, &["enroll", "--name", "laptop"]);

    ok(&admin, &["sync", "--pull"]);
    ok(&admin, &["approve", "laptop", "--yes"]);

    // Approved but not pulled: the laptop honestly still reports pending
    // (its copy is stale) and keeps pointing at `sync --pull`.
    let status = ok(&laptop, &["status"]);
    assert_eq!(status["enrollment"]["state"], "pending", "{status}");
    assert!(
        status["enrollment"]["next"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("sync --pull")),
        "{}",
        status["enrollment"]
    );

    ok(&laptop, &["sync", "--pull"]);
    let status = ok(&laptop, &["status"]);
    assert_eq!(
        status["enrollment"],
        Value::Null,
        "membership must clear the enrollment section: {status}"
    );
    assert!(
        status["machines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "laptop" && m["is_current"] == Value::Bool(true)),
        "approved machine not current after pull: {status}"
    );
    let stdout = text(&laptop, &["status"]);
    assert!(
        stdout.contains("laptop (this machine)"),
        "member not marked current: {stdout}"
    );
    assert!(
        !stdout.contains("Enrollment:"),
        "enrollment block survived membership: {stdout}"
    );
}

#[test]
fn unpublished_enrollment_request_is_distinguished_from_published() {
    let (_remote_guard, remote) = bare_remote();
    let admin = fleet_admin(&remote);
    let signer = support::fleet_signer(&admin.config_dir());
    let laptop = joiner(&remote, &signer);

    // The remote dies after the pull: the enroll push fails, the request is
    // committed locally and preserved — but NOT published.
    let broken = "/nonexistent/muster-unpublished-test.git";
    let status = Command::new("git")
        .args([
            "-C",
            laptop.config_dir().to_str().unwrap(),
            "remote",
            "set-url",
            "origin",
            broken,
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let envelope = fails(&laptop, &["enroll", "--name", "laptop"], 1);
    assert!(
        envelope["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "push_failed"),
        "{envelope}"
    );

    let status = ok(&laptop, &["status"]);
    let enrollment = &status["enrollment"];
    assert_eq!(enrollment["state"], "pending", "{status}");
    assert_eq!(
        enrollment["published"],
        Value::Bool(false),
        "unpublished request reported as published: {status}"
    );
    assert!(
        enrollment["publication_note"]
            .as_str()
            .unwrap()
            .contains("not been pushed"),
        "publication evidence missing: {enrollment}"
    );
    assert!(
        enrollment["next"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("muster sync --push")),
        "publish action missing: {enrollment}"
    );
    let stdout = text(&laptop, &["status"]);
    assert!(
        stdout.contains("Published to the fleet remote: no"),
        "unpublished state not shown: {stdout}"
    );
    assert!(
        stdout.contains("muster sync --push"),
        "publish action missing from text: {stdout}"
    );

    // The admin sees nothing to approve: the request never arrived.
    let status = ok(&admin, &["status"]);
    assert_eq!(
        status["pending"]["machines"].as_array().unwrap().len(),
        0,
        "unpublished request visible to the admin: {status}"
    );
}

#[test]
fn machine_without_a_request_is_told_how_to_enroll() {
    let (_remote_guard, remote) = bare_remote();
    let admin = fleet_admin(&remote);
    let signer = support::fleet_signer(&admin.config_dir());

    // The documented join flow up to the enrollment step: own identity, then
    // pull the fleet (the own solo manifest is replaced by the fleet's). No
    // request exists yet — status must say so and point at `muster enroll`.
    let curious = MusterEnv::new();
    ok(&curious, &["init", "--yes", "--machine", "toolbox"]);
    ok(
        &curious,
        &[
            "sync",
            "--remote",
            &remote,
            "--pull",
            "--expect-signer",
            &signer,
        ],
    );
    let status = ok(&curious, &["status"]);
    let enrollment = &status["enrollment"];
    assert_eq!(enrollment["state"], "none", "{status}");
    assert_eq!(enrollment["member"], false);
    assert!(
        enrollment["next"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("muster enroll")),
        "enroll action missing: {enrollment}"
    );
    let stdout = text(&curious, &["status"]);
    assert!(
        stdout.contains("no enrollment request in the config repo"),
        "no-request state not shown: {stdout}"
    );
    assert!(
        stdout.contains("Next: run `muster enroll`"),
        "enroll next action missing: {stdout}"
    );
}

#[test]
fn enrollment_ownership_is_reported_as_unknown_without_a_local_identity() {
    let (_remote_guard, remote) = bare_remote();
    let admin = fleet_admin(&remote);
    let signer = support::fleet_signer(&admin.config_dir());
    let laptop = joiner(&remote, &signer);
    ok(&laptop, &["enroll", "--name", "laptop"]);

    // The identity disappears (state dir lost): ownership of the request
    // cannot be proven, and status must say so with evidence instead of
    // guessing "pending" or "none".
    std::fs::remove_file(laptop.path().join(".local/state/muster/identity.txt")).unwrap();
    let status = ok(&laptop, &["status"]);
    let enrollment = &status["enrollment"];
    assert_eq!(enrollment["state"], "unknown", "{status}");
    assert!(
        enrollment["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("no local identity")),
        "evidence note missing: {enrollment}"
    );
    let stdout = text(&laptop, &["status"]);
    assert!(
        stdout.contains("cannot be determined"),
        "unknown state not reported: {stdout}"
    );
}

#[test]
fn approval_progress_from_the_quorum_is_reported_honestly() {
    let (_remote_guard, remote) = bare_remote();
    let admin = fleet_admin(&remote);
    let signer = support::fleet_signer(&admin.config_dir());

    // A two-admin fleet with a member to own the joining machine, so that
    // approving it needs two distinct admins.
    ok(&admin, &["member", "add", "worker", "--role", "member"]);
    ok(&admin, &["member", "approve", "worker", "--yes"]);
    let path = admin.config_dir().join("muster.toml");
    let manifest = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        format!("{manifest}\n[team]\nrequired_approvals = 2\n"),
    )
    .unwrap();
    ok(&admin, &["trust", "resign", "--yes"]);
    ok(&admin, &["sync", "--push"]);

    let laptop = joiner(&remote, &signer);
    ok(&laptop, &["enroll", "--name", "laptop"]);

    // First admin approves (1 of 2): the request with its signature is
    // published; the laptop pulls and sees the honest 1-of-2 progress.
    ok(&admin, &["sync", "--pull"]);
    let approved = ok(
        &admin,
        &["approve", "laptop", "--member", "worker", "--yes"],
    );
    assert_eq!(approved["status"], "pending", "{approved}");
    assert_eq!(approved["approvals_received"], 1);
    assert_eq!(approved["approvals_required"], 2);
    ok(&laptop, &["sync", "--pull"]);

    let status = ok(&laptop, &["status"]);
    let enrollment = &status["enrollment"];
    assert_eq!(enrollment["state"], "pending", "{status}");
    assert_eq!(enrollment["approvals_received"], 1, "{enrollment}");
    assert_eq!(enrollment["approvals_required"], 2, "{enrollment}");
    assert_eq!(enrollment["invalid_approvals"], 0, "{enrollment}");
    let stdout = text(&laptop, &["status"]);
    assert!(
        stdout.contains("1 of 2 required approvals"),
        "progress not shown: {stdout}"
    );
    // Still no access claim: the second admin has not approved yet.
    assert!(
        stdout.contains("not a fleet member yet"),
        "no-access line missing: {stdout}"
    );
}
