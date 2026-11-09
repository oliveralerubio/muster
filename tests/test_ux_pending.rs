//! Behavioral regressions for the UX/behavior items validated in research:
//! human `--format text` output, the three secret-policy states, editing an
//! allow policy through the signed circuit, honest agent summaries, provider
//! provenance in `effective`, and the requesting machine's view of pending
//! secret approvals.

mod support;

use std::io::Write;
use std::process::Stdio;

use serde_json::Value;
use support::{Envelope, MusterEnv};

fn ok(env: &MusterEnv, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (out, envelope) = env.run_json(&full);
    assert!(out.status.success(), "{args:?}: {}", envelope.raw);
    assert!(envelope.ok(), "{args:?}: {}", envelope.raw);
    envelope.result().clone()
}

fn fails(env: &MusterEnv, args: &[&str], code: i32) -> Envelope {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (out, envelope) = env.run_json(&full);
    assert_eq!(out.status.code(), Some(code), "{args:?}: {}", envelope.raw);
    assert!(!envelope.ok());
    envelope
}

/// Plain `muster <args>` output (default `--format text`): stdout text,
/// diagnostics on stderr.
fn text(env: &MusterEnv, args: &[&str]) -> (String, String) {
    let out = env.run(args).output().expect("run muster text");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn add_secret(env: &MusterEnv, path: &str, value: &str) -> Value {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("muster"));
    for (k, v) in env.run(&[]).get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    let mut child = cmd
        .args(["--format", "json", "secret", "add", path, "--value-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(value.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "secret add: {stdout}");
    serde_json::from_str::<Value>(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON {e}: {stdout}"))
        .pointer("/result")
        .cloned()
        .unwrap()
}

// ---------------------------------------------------------------------------
// `--format text` human output for status, machine list, secret list, agents.
// JSON, exit codes, stderr and quiet stay unchanged (checked per command).
// ---------------------------------------------------------------------------

#[test]
fn status_machine_list_secret_list_and_agents_render_text_not_json() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    add_secret(&env, "app/api", "s3cr3t");

    let (stdout, stderr) = text(&env, &["status"]);
    assert!(
        !stdout.trim_start().starts_with('{'),
        "status text dumped JSON: {stdout}"
    );
    assert!(
        stdout.contains("box (this machine)"),
        "status text: {stdout}"
    );
    assert!(stdout.contains("Last sync: never"), "status text: {stdout}");
    assert!(stdout.contains("platform "), "status text: {stdout}");
    assert!(
        !stdout.contains("s3cr3t"),
        "status leaked a secret value: {stdout}"
    );
    assert!(stderr.is_empty(), "status text stderr not quiet: {stderr}");

    let (stdout, _) = text(&env, &["machine", "list"]);
    assert!(
        !stdout.trim_start().starts_with('{'),
        "machine list dumped JSON: {stdout}"
    );
    assert!(
        stdout.contains("box (this machine)"),
        "machine list text: {stdout}"
    );
    assert!(
        !stdout.contains("\"recipient\""),
        "machine list text shows raw fields: {stdout}"
    );

    let (stdout, _) = text(&env, &["secret", "list"]);
    assert!(
        !stdout.trim_start().starts_with('{'),
        "secret list dumped JSON: {stdout}"
    );
    assert!(
        stdout.contains("native://app/api"),
        "secret list text: {stdout}"
    );
    assert!(
        stdout.contains("no access policy"),
        "secret list text: {stdout}"
    );
    assert!(
        !stdout.contains("s3cr3t"),
        "secret list leaked a value: {stdout}"
    );

    let (stdout, _) = text(&env, &["agents"]);
    assert!(
        !stdout.trim_start().starts_with('{'),
        "agents dumped JSON: {stdout}"
    );
    assert!(stdout.contains("enabled:"), "agents text: {stdout}");
    assert!(
        stdout.contains("detected on this machine:"),
        "agents text: {stdout}"
    );
    // Honest summary: it must not promise connectivity or authentication.
    assert!(
        stdout.contains("Neither implies"),
        "agents text honesty note: {stdout}"
    );
}

#[test]
fn text_output_covers_empty_and_incomplete_data() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "solo"]);

    // Empty vault: a sentence, not `[]`.
    let (stdout, _) = text(&env, &["secret", "list"]);
    assert_eq!(stdout.trim(), "No secrets in the native vault.", "{stdout}");

    // No pending requests, no leases.
    let (stdout, _) = text(&env, &["secret", "requests"]);
    assert_eq!(stdout.trim(), "No pending secret requests.", "{stdout}");
    let (stdout, _) = text(&env, &["secret", "leases"]);
    assert_eq!(stdout.trim(), "No active secret approvals.", "{stdout}");

    // Incomplete: never synced (no sync-status), machine with no agents.
    let (stdout, _) = text(&env, &["status"]);
    assert!(stdout.contains("Last sync: never"), "{stdout}");
    assert!(stdout.contains("no agents"), "{stdout}");

    // `machine sync` with nothing to sync reports instead of prompting.
    let (_stdout, stderr) = text(&env, &["machine", "sync"]);
    assert!(
        stderr.contains("no remote fleet machines to sync"),
        "machine sync empty stderr: {stderr}"
    );

    // Sync text output summarizes instead of dumping per-file diffs (here
    // without a remote, so there is no transport line to report).
    let (stdout, _) = text(&env, &["sync"]);
    assert!(
        !stdout.contains("\"diff\""),
        "sync text dumped diffs: {stdout}"
    );
    assert!(
        !stdout.contains("\"agents\""),
        "sync text dumped agent JSON: {stdout}"
    );
    assert!(stdout.contains("Applied generation"), "sync text: {stdout}");
}

#[test]
fn text_mode_does_not_change_json_or_exit_codes() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);

    let (json_out, json_env) = env.run_json(&["--format", "json", "status"]);
    assert!(json_out.status.success());
    let machines = json_env.result()["machines"].as_array().unwrap().clone();
    assert_eq!(machines[0]["name"], "box");

    // The same command in text mode keeps exit code 0 and diagnostics on
    // stderr (stderr may carry warnings; stdout carries the summary).
    let out = env.run(&["status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("schema_version"));

    // A failing command keeps its exit code in text mode too.
    let json_out = env
        .run(&[
            "--format",
            "json",
            "secret",
            "get",
            "native://nope/x",
            "--reveal",
        ])
        .output()
        .unwrap();
    let text_out = env
        .run(&["secret", "get", "native://nope/x", "--reveal"])
        .output()
        .unwrap();
    assert_eq!(json_out.status.code(), text_out.status.code());
    assert_ne!(text_out.status.code(), Some(0));
}

// ---------------------------------------------------------------------------
// The three secret-policy states: absent, explicit empty allow, entries.
// They must not be flattened in `secret list`, and `secret allow` moves
// between them while preserving approval and approval_ttl.
// ---------------------------------------------------------------------------

#[test]
fn secret_list_distinguishes_the_three_policy_states() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    add_secret(&env, "app/none", "v");
    add_secret(&env, "app/denied", "v");
    add_secret(&env, "app/shared", "v");
    add_secret(&env, "app/approved", "v");

    ok(
        &env,
        &[
            "secret",
            "allow",
            "native://app/denied",
            "--deny-all",
            "--yes",
        ],
    );
    ok(
        &env,
        &[
            "secret",
            "allow",
            "native://app/shared",
            "--entry",
            "claude",
            "--entry",
            "codex@demo",
            "--yes",
        ],
    );

    let listed = ok(&env, &["secret", "list"]);
    let entries = listed.as_array().unwrap();
    let state = |name: &str| {
        entries
            .iter()
            .find(|e| e["reference"] == format!("native://app/{name}"))
            .unwrap_or_else(|| panic!("missing app/{name} in {listed}"))
            .clone()
    };
    let none = state("none");
    assert_eq!(none["policy_state"], "none");
    assert_eq!(none["approval"], "none");
    let denied = state("denied");
    assert_eq!(denied["policy_state"], "deny_all");
    let shared = state("shared");
    assert_eq!(shared["policy_state"], "allows");
    assert_eq!(
        shared["allow"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>(),
        vec!["claude", "codex@demo"]
    );
}

#[test]
fn secret_allow_preserves_approval_fields_and_supports_all_transitions() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    add_secret(&env, "app/api", "v");

    // absent -> entries, then declare per-use approval on the existing table
    // (string surgery keeps the table unique; `trust resign` re-signs).
    ok(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--entry",
            "claude",
            "--yes",
        ],
    );
    let path = env.config_dir().join("muster.toml");
    let manifest = std::fs::read_to_string(&path).unwrap();
    let manifest = manifest.replace(
        "approval = \"none\"",
        "approval = \"each\"\napproval_ttl = \"30m\"",
    );
    std::fs::write(&path, manifest).unwrap();
    ok(&env, &["trust", "resign", "--yes"]);

    // entries -> deny_all: approval fields preserved.
    let result = ok(
        &env,
        &["secret", "allow", "native://app/api", "--deny-all", "--yes"],
    );
    assert_eq!(result["policy_state"], "deny_all");
    assert_eq!(result["approval"], "each");
    assert_eq!(result["approval_ttl"], "30m");
    let manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(
        manifest.contains("approval = \"each\""),
        "approval lost: {manifest}"
    );
    assert!(
        manifest.contains("approval_ttl = \"30m\""),
        "ttl lost: {manifest}"
    );
    assert!(
        !manifest.contains("allow = [\"claude\"]"),
        "old entries kept: {manifest}"
    );

    // deny_all -> entries again (replaces the whole list).
    let result = ok(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--entry",
            "codex",
            "--entry",
            "pi@proj",
            "--yes",
        ],
    );
    assert_eq!(result["policy_state"], "allows");
    let manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(
        manifest.contains("approval = \"each\""),
        "approval lost on set: {manifest}"
    );

    // entries -> no allow list. Only the allow list goes: the approval
    // policy (approval=each, ttl=30m) survives the removal untouched.
    let result = ok(
        &env,
        &["secret", "allow", "native://app/api", "--remove", "--yes"],
    );
    assert_eq!(result["policy_state"], "approval_only");
    assert_eq!(result["approval"], "each");
    assert_eq!(result["approval_ttl"], "30m");
    let manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(
        manifest.contains("[secrets.\"native://app/api\"]"),
        "policy entry dropped while it still carries approval: {manifest}"
    );
    assert!(
        manifest.contains("approval = \"each\"") && manifest.contains("approval_ttl = \"30m\""),
        "approval policy lost on remove: {manifest}"
    );
    assert!(
        !manifest.contains("allow = "),
        "allow list kept after --remove: {manifest}"
    );
    // `secret list` reports the fourth state (not flattened into "none").
    let listed = ok(&env, &["secret", "list"]);
    assert_eq!(listed[0]["policy_state"], "approval_only", "{listed}");
    assert_eq!(listed[0]["approval"], "each", "{listed}");
    let (stdout, _) = text(&env, &["secret", "list"]);
    assert!(
        stdout.contains("no allow list (agents never receive it)"),
        "list text hides the approval-only state: {stdout}"
    );
    assert!(
        stdout.contains("per-use approval, ttl 30m"),
        "list text hides the approval policy: {stdout}"
    );
    // The per-use approval requirement still gates reveals after --remove:
    // removing the allow list cannot remove the approval requirement. An
    // unnamed caller still trips the policy, and a named agent outside the
    // (now absent) allow list is still refused.
    let envelope = fails(
        &env,
        &["secret", "get", "native://app/api", "--reveal", "--yes"],
        5,
    );
    assert!(
        envelope.raw["diagnostics"][0]["code"] == "secret_policy_requires_agent",
        "approval policy no longer constrains unnamed reveals: {}",
        envelope.raw
    );
    let envelope = fails(
        &env,
        &[
            "secret",
            "get",
            "native://app/api",
            "--agent",
            "claude",
            "--reveal",
            "--yes",
        ],
        5,
    );
    assert!(
        envelope.raw["diagnostics"][0]["code"] == "secret_not_allowed_for_agent",
        "approval-only policy stopped gating agent reveals: {}",
        envelope.raw
    );

    // deny_all -> absent: with the approval policy also gone (declared
    // "none" again), --remove takes the whole entry down.
    let path = env.config_dir().join("muster.toml");
    let manifest = std::fs::read_to_string(&path).unwrap();
    let manifest = manifest.replace(
        "approval = \"each\"\napproval_ttl = \"30m\"",
        "approval = \"none\"",
    );
    std::fs::write(&path, manifest).unwrap();
    ok(&env, &["trust", "resign", "--yes"]);
    ok(
        &env,
        &["secret", "allow", "native://app/api", "--deny-all", "--yes"],
    );
    let result = ok(
        &env,
        &["secret", "allow", "native://app/api", "--remove", "--yes"],
    );
    assert_eq!(result["policy_state"], "none");
    assert_eq!(result["approval"], "none");
    let manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(
        !manifest.contains("[secrets."),
        "policy table kept: {manifest}"
    );
}

#[test]
fn secret_allow_remove_distinguishes_absent_approval_only_and_empty_policies() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    add_secret(&env, "app/api", "v");

    // Absent policy: teaches instead of no-op (kept from before).
    let envelope = fails(
        &env,
        &["secret", "allow", "native://app/api", "--remove", "--yes"],
        2,
    );
    assert!(
        envelope.raw["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("no access policy declared"),
        "{}",
        envelope.raw
    );

    // An approval-only policy (no allow list, approval=each + ttl declared by
    // hand) is NOT "no policy": --remove refuses and names what it holds.
    ok(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--entry",
            "claude",
            "--yes",
        ],
    );
    let path = env.config_dir().join("muster.toml");
    let manifest = std::fs::read_to_string(&path).unwrap();
    let manifest = manifest.replace("allow = [\"claude\"]\n", "").replace(
        "approval = \"none\"",
        "approval = \"each\"\napproval_ttl = \"45m\"",
    );
    std::fs::write(&path, manifest).unwrap();
    ok(&env, &["trust", "resign", "--yes"]);

    let listed = ok(&env, &["secret", "list"]);
    assert_eq!(listed[0]["policy_state"], "approval_only", "{listed}");

    let envelope = fails(
        &env,
        &["secret", "allow", "native://app/api", "--remove", "--yes"],
        2,
    );
    let message = envelope.raw["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        message.contains("no allow list") && message.contains("approval=each, ttl=45m"),
        "approval-only policy not distinguished: {message}"
    );
    // Refused, so the approval policy is still exactly there.
    let manifest = std::fs::read_to_string(&path).unwrap();
    assert!(
        manifest.contains("approval = \"each\"") && manifest.contains("approval_ttl = \"45m\""),
        "refusal still changed the manifest: {manifest}"
    );

    // An empty policy entry (no allow list, no approval fields) is its own
    // state too: refused with its own words, not the absent-policy ones.
    let manifest = manifest.replace(
        "approval = \"each\"\napproval_ttl = \"45m\"",
        "approval = \"none\"",
    );
    std::fs::write(&path, manifest).unwrap();
    ok(&env, &["trust", "resign", "--yes"]);
    let envelope = fails(
        &env,
        &["secret", "allow", "native://app/api", "--remove", "--yes"],
        2,
    );
    let message = envelope.raw["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        message.contains("empty") && !message.contains("no access policy declared"),
        "empty policy not distinguished: {message}"
    );
}

#[test]
fn secret_allow_remove_confirmation_and_help_describe_the_exact_change() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    add_secret(&env, "app/api", "v");
    ok(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--entry",
            "claude",
            "--yes",
        ],
    );
    let path = env.config_dir().join("muster.toml");
    let manifest = std::fs::read_to_string(&path).unwrap();
    let manifest = manifest.replace("approval = \"none\"", "approval = \"each\"");
    std::fs::write(&path, manifest).unwrap();
    ok(&env, &["trust", "resign", "--yes"]);

    // No --yes and no terminal: the confirmation question itself is the
    // error, so its exact wording is testable. With an approval policy
    // present it must promise the approval policy survives.
    let envelope = fails(
        &env,
        &["secret", "allow", "native://app/api", "--remove"],
        2,
    );
    let question = envelope.raw["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        question.contains("Remove only the allow list"),
        "question does not scope the removal to the allow list: {question}"
    );
    assert!(
        question.contains("approval policy (approval=each) is preserved"),
        "question does not teach the approval survival: {question}"
    );
    assert!(
        !question.contains("approval and approval_ttl go with it"),
        "question still promises the old destructive behavior: {question}"
    );
    // Nothing changed: no policy was removed by the refused confirmation.
    let manifest = std::fs::read_to_string(&path).unwrap();
    assert!(manifest.contains("allow = [\"claude\"]"), "{manifest}");

    // Without an approval policy the question says the whole entry goes.
    let manifest = manifest.replace("approval = \"each\"", "approval = \"none\"");
    std::fs::write(&path, manifest).unwrap();
    ok(&env, &["trust", "resign", "--yes"]);
    let envelope = fails(
        &env,
        &["secret", "allow", "native://app/api", "--remove"],
        2,
    );
    let question = envelope.raw["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        question.contains("Remove the access policy"),
        "whole-entry removal not described as such: {question}"
    );

    // The dry run predicts the state it would leave behind, including the
    // approval-only outcome of removing an allow list under an approval
    // policy.
    let manifest = std::fs::read_to_string(&path).unwrap();
    let manifest = manifest.replace(
        "approval = \"none\"",
        "approval = \"each\"\napproval_ttl = \"20m\"",
    );
    std::fs::write(&path, manifest).unwrap();
    ok(&env, &["trust", "resign", "--yes"]);
    let result = ok(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--remove",
            "--yes",
            "--dry-run",
        ],
    );
    assert_eq!(result["policy_state"], "approval_only", "{result}");
    let manifest = std::fs::read_to_string(&path).unwrap();
    assert!(
        manifest.contains("allow = [\"claude\"]"),
        "dry run mutated the manifest: {manifest}"
    );
}

#[test]
fn secret_allow_remove_is_covered_by_the_signed_fleet_state() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    add_secret(&env, "app/api", "v");
    ok(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--entry",
            "claude",
            "--yes",
        ],
    );

    // Applied removal, then a hand edit re-adds the allow list: the security
    // check must trip, proving the removal was sealed into the signed state.
    ok(
        &env,
        &["secret", "allow", "native://app/api", "--remove", "--yes"],
    );
    let path = env.config_dir().join("muster.toml");
    let manifest = std::fs::read_to_string(&path).unwrap();
    assert!(
        !manifest.contains("[secrets."),
        "entry should be fully gone (no approval fields): {manifest}"
    );
    let manifest = format!("{manifest}\n[secrets.\"native://app/api\"]\nallow = [\"claude\"]\n");
    std::fs::write(&path, manifest).unwrap();
    let envelope = fails(&env, &["status"], 5);
    assert!(
        envelope.raw["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("do not match the signed fleet state"),
        "allow-list removal is not inside the signature: {}",
        envelope.raw
    );
}

#[test]
fn secret_allow_validates_entries_and_reference() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    add_secret(&env, "app/api", "v");

    // Unknown agent: the shared `validate_allow_entry` rejection.
    let envelope = fails(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--entry",
            "nope",
            "--yes",
        ],
        2,
    );
    assert!(
        envelope.raw["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("unknown agent `nope`"),
        "{}",
        envelope.raw
    );

    // Malformed project qualifier.
    fails(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--entry",
            "claude@a/b",
            "--yes",
        ],
        2,
    );

    // Invalid reference.
    fails(
        &env,
        &[
            "secret",
            "allow",
            "native://onlygroup",
            "--entry",
            "claude",
            "--yes",
        ],
        2,
    );

    // No mode chosen (exactly one of entry/deny-all/remove).
    fails(&env, &["secret", "allow", "native://app/api", "--yes"], 2);
    // Two modes at once.
    fails(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--deny-all",
            "--remove",
            "--yes",
        ],
        2,
    );

    // Removing a policy that does not exist teaches instead of no-op.
    let envelope = fails(
        &env,
        &["secret", "allow", "native://app/api", "--remove", "--yes"],
        2,
    );
    assert!(
        envelope.raw["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("no access policy declared"),
        "{}",
        envelope.raw
    );
}

#[test]
fn secret_allow_without_confirmation_changes_nothing() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    add_secret(&env, "app/api", "v");

    // No terminal, no --yes: refused, and neither manifest nor policy change.
    let envelope = fails(
        &env,
        &["secret", "allow", "native://app/api", "--entry", "claude"],
        2,
    );
    assert!(
        envelope.raw["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("confirmation required"),
        "{}",
        envelope.raw
    );
    let listed = ok(&env, &["secret", "list"]);
    assert_eq!(listed[0]["policy_state"], "none", "{listed}");
    let manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(!manifest.contains("[secrets."), "{manifest}");
}

#[test]
fn secret_allow_confirmed_signs_and_commits_the_fleet_state() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    add_secret(&env, "app/api", "v");

    let result = ok(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--entry",
            "claude",
            "--yes",
        ],
    );
    assert_eq!(result["committed"], true, "{result}");
    assert_eq!(
        result["quorum"],
        Value::Null,
        "single-admin fleet applies at once: {result}"
    );

    // The signed fleet state now covers the new policy (a hand edit would
    // trip the security check, proving the policy is inside the signature).
    let manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(manifest.contains("allow = [\"claude\"]"), "{manifest}");
    let tampered = manifest.replace("claude", "codex");
    std::fs::write(env.config_dir().join("muster.toml"), tampered).unwrap();
    let envelope = fails(&env, &["status"], 5);
    assert!(
        envelope.raw["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("do not match the signed fleet state"),
        "{}",
        envelope.raw
    );
}

// ---------------------------------------------------------------------------
// Quorum: a policy change short of admin co-signatures stays a pending
// proposal. The pin must not advance and the change must not be applied.
// ---------------------------------------------------------------------------

fn init_quorum_fleet(env: &MusterEnv) {
    ok(env, &["init", "--yes", "--machine", "alpha"]);
    // A second admin, so 2 signatures are needed for governance changes.
    ok(env, &["member", "add", "bob", "--role", "admin"]);
    ok(env, &["member", "approve", "bob", "--yes"]);
    let path = env.config_dir().join("muster.toml");
    let manifest = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        format!("{manifest}\n[team]\nrequired_approvals = 2\n"),
    )
    .unwrap();
    ok(env, &["trust", "resign", "--yes"]);
}

#[test]
fn secret_allow_under_quorum_stays_pending_without_pinning_or_applying() {
    let env = MusterEnv::new();
    init_quorum_fleet(&env);
    add_secret(&env, "app/api", "v");

    let result = ok(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--entry",
            "claude",
            "--yes",
        ],
    );
    let quorum = &result["quorum"];
    assert_eq!(quorum["pending"], true, "{result}");
    assert_eq!(quorum["received"], 1, "{result}");
    assert_eq!(quorum["required"], 2, "{result}");

    // The pinned-but-unaccepted state refuses the next security-consuming
    // command instead of laundering it (`secret list` reads names only and
    // stays available; `status` consumes the fleet state).
    let envelope = fails(&env, &["status"], 5);
    let message = envelope.raw["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        message.contains("required admin signatures") && message.contains("NOT accepted"),
        "pending change was accepted: {message}"
    );

    // The signed state on disk is the proposal: one epoch ahead of the pin.
    let pin: Value = serde_json::from_str(
        &std::fs::read_to_string(env.path().join(".local/state/muster/trust.json")).unwrap(),
    )
    .unwrap();
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(env.config_dir().join("fleet.state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        state["payload"]["epoch"].as_u64().unwrap(),
        pin["epoch"].as_u64().unwrap() + 1,
        "the pending proposal must sit one epoch ahead of the pin"
    );
}

#[test]
fn secret_allow_remove_under_quorum_preserves_approval_and_stays_pending() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "alpha"]);
    add_secret(&env, "app/api", "v");

    // An applied policy with both an allow list and an approval policy,
    // declared while the quorum is still 1 (applied at once), then sealed.
    ok(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--entry",
            "claude",
            "--yes",
        ],
    );
    let path = env.config_dir().join("muster.toml");
    let manifest = std::fs::read_to_string(&path).unwrap();
    let manifest = manifest.replace(
        "approval = \"none\"",
        "approval = \"each\"\napproval_ttl = \"30m\"",
    );
    std::fs::write(&path, manifest).unwrap();
    ok(&env, &["trust", "resign", "--yes"]);

    // Raise the quorum to 2 (a second admin exists), so the removal below
    // is a governance change one signature short.
    ok(&env, &["member", "add", "bob", "--role", "admin"]);
    ok(&env, &["member", "approve", "bob", "--yes"]);
    let manifest = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        format!("{manifest}\n[team]\nrequired_approvals = 2\n"),
    )
    .unwrap();
    ok(&env, &["trust", "resign", "--yes"]);

    // The removal is a proposal: one admin signature of two. Even as a
    // proposal it must preserve the approval policy — the old whole-entry
    // removal would have deleted it from the proposal manifest too.
    let result = ok(
        &env,
        &["secret", "allow", "native://app/api", "--remove", "--yes"],
    );
    assert_eq!(result["quorum"]["pending"], true, "{result}");
    assert_eq!(result["quorum"]["received"], 1, "{result}");
    assert_eq!(result["quorum"]["required"], 2, "{result}");
    let manifest = std::fs::read_to_string(&path).unwrap();
    assert!(
        !manifest.contains("allow = "),
        "proposal manifest still allows agents: {manifest}"
    );
    assert!(
        manifest.contains("approval = \"each\"") && manifest.contains("approval_ttl = \"30m\""),
        "pending proposal lost the approval policy: {manifest}"
    );

    // The pinned-but-unaccepted state refuses the next security-consuming
    // command instead of laundering the proposal, and the proposal sits one
    // epoch ahead of the pin.
    let envelope = fails(&env, &["status"], 5);
    let message = envelope.raw["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        message.contains("required admin signatures") && message.contains("NOT accepted"),
        "pending removal was accepted: {message}"
    );
    let pin: Value = serde_json::from_str(
        &std::fs::read_to_string(env.path().join(".local/state/muster/trust.json")).unwrap(),
    )
    .unwrap();
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(env.config_dir().join("fleet.state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        state["payload"]["epoch"].as_u64().unwrap(),
        pin["epoch"].as_u64().unwrap() + 1,
        "the pending removal must sit one epoch ahead of the pin"
    );
}

#[test]
fn secret_allow_on_a_machine_without_signing_authority_is_refused() {
    // The admin machine owns a bare remote with one secret.
    let remote_dir = tempfile::TempDir::new().unwrap();
    let remote = remote_dir.path().join("origin.git");
    let init = std::process::Command::new("git")
        .args(["init", "--bare", "--", remote.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let remote_url = remote.to_str().unwrap().to_owned();

    let admin = MusterEnv::new();
    ok(&admin, &["init", "--yes", "--machine", "admin-box"]);
    add_secret(&admin, "app/api", "v");
    ok(&admin, &["sync", "--remote", &remote_url, "--push"]);
    let signer = support::fleet_signer(&admin.config_dir());

    // A second machine joins (pulls and pins) but is NOT approved: it holds
    // no signing authority over the fleet state.
    let guest = MusterEnv::new();
    ok(
        &guest,
        &[
            "sync",
            "--remote",
            &remote_url,
            "--pull",
            "--expect-signer",
            &signer,
        ],
    );
    let envelope = fails(
        &guest,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--entry",
            "claude",
            "--yes",
        ],
        5,
    );
    let message = envelope.raw["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        message.contains("cannot sign the fleet state"),
        "no-authority refusal missing: {message}"
    );

    // Nothing changed anywhere: the guest manifest is untouched and the
    // admin still sees no policy.
    let manifest = std::fs::read_to_string(guest.config_dir().join("muster.toml")).unwrap();
    assert!(
        !manifest.contains("[secrets."),
        "guest wrote a policy anyway: {manifest}"
    );
    let listed = ok(&admin, &["secret", "list"]);
    assert_eq!(listed[0]["policy_state"], "none", "{listed}");
}

// ---------------------------------------------------------------------------
// The requesting machine sees its pending secret approvals in `status`.
// ---------------------------------------------------------------------------

#[test]
fn pending_secret_requests_surface_in_status() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    add_secret(&env, "app/api", "v");
    ok(
        &env,
        &[
            "secret",
            "allow",
            "native://app/api",
            "--entry",
            "claude",
            "--yes",
        ],
    );

    // Declare per-use approval and let an agent ask (fails closed, queued).
    let path = env.config_dir().join("muster.toml");
    let manifest = std::fs::read_to_string(&path).unwrap();
    let manifest = manifest.replace("approval = \"none\"", "approval = \"each\"");
    std::fs::write(&path, manifest).unwrap();
    ok(&env, &["trust", "resign", "--yes"]);
    let envelope = fails(
        &env,
        &[
            "secret",
            "get",
            "native://app/api",
            "--agent",
            "claude",
            "--reveal",
            "--yes",
        ],
        5,
    );
    assert!(
        envelope.raw["diagnostics"][0]["code"] == "secret_approval_required",
        "{}",
        envelope.raw
    );

    let status = ok(&env, &["status"]);
    let pending = status["pending_secret_approvals"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(pending.len(), 1, "{status}");
    assert_eq!(pending[0]["reference"], "native://app/api");
    assert_eq!(pending[0]["agent"], "claude");
    assert!(pending[0]["id"].as_str().is_some());

    // Text mode names the exact approval command.
    let (stdout, _) = text(&env, &["status"]);
    assert!(stdout.contains("Pending secret approvals"), "{stdout}");
    assert!(stdout.contains("muster secret approve"), "{stdout}");

    // Once approved (or denied), status is quiet again.
    let id = pending[0]["id"].as_str().unwrap().to_string();
    ok(&env, &["secret", "deny", &id, "--yes"]);
    let status = ok(&env, &["status"]);
    assert_eq!(
        status["pending_secret_approvals"].as_array().unwrap().len(),
        0,
        "{status}"
    );
}

// ---------------------------------------------------------------------------
// `effective` reports AI providers (metadata + winning layer, no values).
// ---------------------------------------------------------------------------

#[test]
fn effective_reports_provider_metadata_and_origin() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    ok(
        &env,
        &[
            "provider",
            "add",
            "acme",
            "--kind",
            "openai-compatible",
            "--base-url",
            "https://acme.test",
            "--default-model",
            "acme-1",
            "--key",
            "native://app/api",
            "--yes",
        ],
    );

    let result = ok(&env, &["effective"]);
    let providers = result["providers"].as_array().unwrap().clone();
    let acme = providers
        .iter()
        .find(|p| p["id"] == "acme")
        .unwrap_or_else(|| panic!("provider missing from effective: {result}"));
    assert_eq!(acme["kind"], "openai-compatible");
    assert_eq!(acme["base_url"], "https://acme.test");
    assert_eq!(acme["default_model"], "acme-1");
    // Keys appear as references only, never values.
    assert_eq!(acme["key"], "native://app/api");
    assert!(acme["layer"].as_str().is_some(), "no origin layer: {acme}");

    // The panel path returns the same JSON (the UI forces JSON itself).
    let panel_style = ok(&env, &["effective"]);
    assert_eq!(panel_style["providers"], result["providers"]);

    // Text mode lists providers with their origin.
    let (stdout, _) = text(&env, &["effective"]);
    assert!(stdout.contains("AI providers"), "{stdout}");
    assert!(stdout.contains("acme"), "{stdout}");
}

// ---------------------------------------------------------------------------
// A present-but-broken muster.toml is not reported as "missing".
// ---------------------------------------------------------------------------

#[test]
fn broken_manifest_error_distinguishes_unreadable_from_missing() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    let path = env.config_dir().join("muster.toml");
    let original = std::fs::read_to_string(&path).unwrap();

    std::fs::write(&path, format!("{original}\n[secrets.broken\n")).unwrap();
    let (stdout, stderr) = text(&env, &["status"]);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("cannot be read") || combined.contains("unreadable"),
        "broken manifest message: {combined}"
    );
    assert!(
        !combined.contains("is missing ("),
        "broken manifest reported as missing: {combined}"
    );

    // Missing is still its own situation.
    std::fs::remove_file(&path).unwrap();
    let (stdout, stderr) = text(&env, &["status"]);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("is missing ("),
        "missing manifest message: {combined}"
    );
}

// ---------------------------------------------------------------------------
// History keeps the local actor of events; unknown for older records.
// ---------------------------------------------------------------------------

#[test]
fn history_events_carry_the_recorded_actor() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "box"]);
    add_secret(&env, "app/api", "v");

    let events = ok(&env, &["history"]);
    let events = events["events"].as_array().unwrap().clone();
    assert!(
        events.iter().any(|e| e["actor"] == "box"),
        "local events do not name the machine actor: {events:?}"
    );
}
