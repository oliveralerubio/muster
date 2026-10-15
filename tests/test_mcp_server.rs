//! Editing agent configuration from the CLI and the web panel (SPEC story 56):
//! `mcp-server add|rm|list` for the manifest's `[mcp]` table and `profile set
//! --toml` / `profile show --toml` for inline profile text, both with
//! `--dry-run` diffs that write nothing.
mod support;

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

fn init(env: &MusterEnv) {
    ok(env, &["init", "--yes", "--machine", "laptop"]);
}

fn manifest(env: &MusterEnv) -> String {
    std::fs::read_to_string(env.config_dir().join("muster.toml")).expect("read muster.toml")
}

#[test]
fn mcp_server_add_list_and_rm_edit_the_manifest() {
    let env = MusterEnv::new();
    init(&env);

    let added = ok(
        &env,
        &[
            "mcp-server",
            "add",
            "context",
            "--command",
            "npx",
            "--args=-y",
            "--args",
            "@modelcontextprotocol/server-context",
            "--env",
            "NODE_ENV=production",
            "--env",
            "API_KEY=native://app/api-key",
            "--yes",
        ],
    );
    assert_eq!(added["transport"], "stdio");
    assert_eq!(added["server"]["command"], "npx");
    assert_eq!(added["server"]["env"]["API_KEY"], "native://app/api-key");
    assert!(manifest(&env).contains("[mcp.context]"));

    let remote = ok(
        &env,
        &[
            "mcp-server",
            "add",
            "docs",
            "--url",
            "https://docs.example/mcp",
            "--header",
            "Authorization=native://app/docs-token",
            "--yes",
        ],
    );
    assert_eq!(remote["transport"], "remote");
    assert_eq!(
        remote["server"]["headers"]["Authorization"],
        "native://app/docs-token"
    );

    let listed = ok(&env, &["mcp-server", "list"]);
    let names: Vec<_> = listed["servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].clone())
        .collect();
    assert_eq!(
        names,
        vec![serde_json::json!("context"), serde_json::json!("docs")]
    );

    // Adding again replaces the entry in place.
    let replaced = ok(
        &env,
        &["mcp-server", "add", "context", "--command", "node", "--yes"],
    );
    assert_eq!(replaced["replaced"], true);
    assert_eq!(replaced["server"]["command"], "node");
    assert_eq!(
        ok(&env, &["mcp-server", "list"])["servers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Removal asks for confirmation: without --yes it fails and keeps the entry.
    fails(&env, &["mcp-server", "rm", "docs"], 2);
    assert!(manifest(&env).contains("[mcp.docs]"));
    let removed = ok(&env, &["mcp-server", "rm", "docs", "--yes"]);
    assert_eq!(removed["removed"], "docs");
    assert!(!manifest(&env).contains("[mcp.docs]"));
    fails(&env, &["mcp-server", "rm", "docs", "--yes"], 2);
}

#[test]
fn mcp_server_add_rejects_invalid_servers_with_stable_exit_codes() {
    let env = MusterEnv::new();
    init(&env);

    // Neither command nor url: invalid config (exit 3).
    fails(&env, &["mcp-server", "add", "broken", "--yes"], 3);
    // Both at once: also invalid config.
    fails(
        &env,
        &[
            "mcp-server",
            "add",
            "broken",
            "--command",
            "npx",
            "--url",
            "https://x",
        ],
        3,
    );
    // Bad name (must be lowercase/digits/-): usage (exit 2).
    fails(&env, &["mcp-server", "add", "Big", "--command", "npx"], 2);
    // env entries must be KEY=VALUE: usage (exit 2).
    fails(
        &env,
        &[
            "mcp-server",
            "add",
            "envy",
            "--command",
            "npx",
            "--env",
            "NOKEY",
        ],
        2,
    );
    // Nothing was written.
    assert!(!manifest(&env).contains("[mcp."), "{}", manifest(&env));
}

#[test]
fn mcp_server_add_dry_run_shows_diff_and_writes_nothing() {
    let env = MusterEnv::new();
    init(&env);
    ok(
        &env,
        &["mcp-server", "add", "context", "--command", "npx", "--yes"],
    );
    let before = manifest(&env);

    // Replacing with a dry run: diff against the stored server, no write.
    let dry = ok(
        &env,
        &[
            "--dry-run",
            "mcp-server",
            "add",
            "context",
            "--command",
            "node",
            "--args",
            "server.js",
        ],
    );
    assert_eq!(dry["replaced"], true);
    let diff = dry["diff"].as_str().expect("dry run returns a diff");
    assert!(diff.contains("-command = \"npx\""), "{diff}");
    assert!(diff.contains("+command = \"node\""), "{diff}");
    assert!(diff.contains("+args ="), "{diff}");
    assert_eq!(manifest(&env), before, "dry run must not write");

    // A brand-new server dry run diffs against nothing.
    let fresh = ok(
        &env,
        &[
            "--dry-run",
            "mcp-server",
            "add",
            "docs",
            "--url",
            "https://x",
        ],
    );
    assert_eq!(fresh["replaced"], false);
    assert!(
        fresh["diff"].as_str().unwrap().contains("+url = "),
        "{fresh}"
    );
    assert_eq!(manifest(&env), before);
}

#[test]
fn mcp_server_add_preserves_unrelated_manifest_content() {
    let env = MusterEnv::new();
    init(&env);
    let file = env.path().join("dev.toml");
    std::fs::write(&file, "[agents.models]\nclaude = \"sonnet\"\n").unwrap();
    ok(
        &env,
        &[
            "profile",
            "set",
            "dev",
            "--from-file",
            file.to_str().unwrap(),
            "--yes",
        ],
    );
    let toml = manifest(&env);
    let with_extension = format!("{toml}\nx-note = \"keep me\"\n");
    std::fs::write(env.config_dir().join("muster.toml"), with_extension).unwrap();

    ok(
        &env,
        &["mcp-server", "add", "context", "--command", "npx", "--yes"],
    );
    let after = manifest(&env);
    assert!(after.contains("[profiles.dev"), "{after}");
    assert!(after.contains("x-note = \"keep me\""), "{after}");

    ok(&env, &["mcp-server", "rm", "context", "--yes"]);
    let after = manifest(&env);
    assert!(after.contains("[profiles.dev"), "{after}");
    assert!(after.contains("x-note = \"keep me\""), "{after}");
}

#[test]
fn mcp_server_changes_are_admin_only_in_team_mode() {
    let owner = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir().to_str().unwrap().to_owned();
    ok(&owner, &["member", "add", "alice", "--role", "member"]);
    ok(&owner, &["member", "approve", "alice", "--yes"]);
    ok(
        &owner,
        &["mcp-server", "add", "context", "--command", "npx", "--yes"],
    );

    let alice = MusterEnv::new();
    ok(
        &alice,
        &["--config", &config, "enroll", "--name", "alice-box"],
    );
    ok(
        &owner,
        &["approve", "alice-box", "--member", "alice", "--yes"],
    );
    let denied = fails(
        &alice,
        &[
            "--config",
            &config,
            "mcp-server",
            "add",
            "other",
            "--command",
            "npx",
            "--yes",
        ],
        5,
    );
    assert!(denied.raw.to_string().contains("admin"), "{}", denied.raw);
    fails(
        &alice,
        &["--config", &config, "mcp-server", "rm", "context", "--yes"],
        5,
    );
}

#[test]
fn profile_set_toml_edits_inline_and_show_toml_round_trips() {
    let env = MusterEnv::new();
    init(&env);

    let set = ok(
        &env,
        &[
            "profile",
            "set",
            "dev",
            "--toml",
            "[agents.models]\nclaude = \"sonnet\"\n",
            "--yes",
        ],
    );
    assert_eq!(set["profile"]["agents"]["models"]["claude"], "sonnet");
    assert!(manifest(&env).contains("[profiles.dev"));

    let shown = ok(&env, &["profile", "show", "dev", "--toml"]);
    assert_eq!(
        shown["toml"].as_str().unwrap(),
        "[agents.models]\nclaude = \"sonnet\"\n"
    );

    // Passing both sources is a usage error; so is passing neither.
    let file = env.path().join("dev.toml");
    std::fs::write(&file, "[agents.models]\nclaude = \"opus\"\n").unwrap();
    fails(
        &env,
        &[
            "profile",
            "set",
            "dev",
            "--from-file",
            file.to_str().unwrap(),
            "--toml",
            "[agents.models]\nclaude = \"haiku\"\n",
        ],
        2,
    );
    fails(&env, &["profile", "set", "other"], 2);
    // Invalid inline TOML is a config error (exit 3) and writes nothing.
    fails(&env, &["profile", "set", "dev", "--toml", "bogus = 1\n"], 3);
    let shown = ok(&env, &["profile", "show", "dev", "--toml"]);
    assert!(shown["toml"].as_str().unwrap().contains("sonnet"));
}

#[test]
fn profile_set_dry_run_diffs_against_the_stored_profile() {
    let env = MusterEnv::new();
    init(&env);
    ok(
        &env,
        &[
            "profile",
            "set",
            "dev",
            "--toml",
            "[agents.models]\nclaude = \"sonnet\"\n",
            "--yes",
        ],
    );
    let before = manifest(&env);

    let dry = ok(
        &env,
        &[
            "--dry-run",
            "profile",
            "set",
            "dev",
            "--toml",
            "[agents.models]\nclaude = \"opus\"\n",
        ],
    );
    let diff = dry["diff"].as_str().expect("dry run returns a diff");
    assert!(diff.contains("-claude = \"sonnet\""), "{diff}");
    assert!(diff.contains("+claude = \"opus\""), "{diff}");
    assert_eq!(manifest(&env), before, "dry run must not write");

    // A new profile diff shows the whole profile as added.
    let fresh = ok(
        &env,
        &[
            "--dry-run",
            "profile",
            "set",
            "edge",
            "--toml",
            "[agents]\nenabled = [\"claude\"]\n",
        ],
    );
    assert!(
        fresh["diff"].as_str().unwrap().contains("+[agents]"),
        "{fresh}"
    );
    assert_eq!(manifest(&env), before);
}

#[test]
fn mcp_server_add_requires_confirmation_and_dry_run_does_not() {
    let env = MusterEnv::new();
    init(&env);

    // Applying without --yes (and without a terminal) is refused, exit 2,
    // and writes nothing.
    fails(
        &env,
        &["mcp-server", "add", "context", "--command", "npx"],
        2,
    );
    assert!(
        !manifest(&env).contains("[mcp.context]"),
        "{}",
        manifest(&env)
    );

    // `--yes` applies, like for every other mutating command.
    ok(
        &env,
        &["mcp-server", "add", "context", "--command", "npx", "--yes"],
    );
    assert!(manifest(&env).contains("[mcp.context]"));

    // A dry-run preview needs no confirmation.
    let dry = ok(
        &env,
        &[
            "--dry-run",
            "mcp-server",
            "add",
            "docs",
            "--url",
            "https://x",
        ],
    );
    assert!(dry["diff"].as_str().unwrap().contains("+url"), "{dry}");
    assert!(!manifest(&env).contains("[mcp.docs]"), "{}", manifest(&env));
}

#[test]
fn profile_set_requires_confirmation_and_dry_run_does_not() {
    let env = MusterEnv::new();
    init(&env);

    // Applying without --yes is refused and writes nothing.
    fails(
        &env,
        &[
            "profile",
            "set",
            "dev",
            "--toml",
            "[agents]\nenabled = []\n",
        ],
        2,
    );
    assert!(
        !manifest(&env).contains("[profiles.dev"),
        "{}",
        manifest(&env)
    );

    ok(
        &env,
        &[
            "profile",
            "set",
            "dev",
            "--toml",
            "[agents]\nenabled = []\n",
            "--yes",
        ],
    );
    assert!(
        manifest(&env).contains("[profiles.dev"),
        "{}",
        manifest(&env)
    );

    // A dry-run preview needs no confirmation.
    let dry = ok(
        &env,
        &[
            "--dry-run",
            "profile",
            "set",
            "dev",
            "--toml",
            "[agents.models]\nclaude = \"opus\"\n",
        ],
    );
    assert!(dry["diff"].as_str().unwrap().contains("+claude"), "{dry}");
    assert!(!manifest(&env).contains("opus"), "{}", manifest(&env));
}

#[test]
fn list_params_do_not_swallow_leading_global_flags() {
    let env = MusterEnv::new();
    init(&env);

    // `allow_hyphen_values` is gone: a value like `--dry-run` or `--yes` after
    // `--args` is clap's "value required" usage error (exit 2), never a
    // swallowed global flag applied with args=["--dry-run"].
    for flag in ["--dry-run", "--yes", "--quiet"] {
        fails(
            &env,
            &["mcp-server", "add", "x", "--command", "npx", "--args", flag],
            2,
        );
    }
    assert!(!manifest(&env).contains("[mcp.x]"), "{}", manifest(&env));

    // Hyphen-leading values use the `=` form; plain values keep the space form.
    let added = ok(
        &env,
        &[
            "mcp-server",
            "add",
            "x",
            "--command",
            "npx",
            "--args=-y",
            "--args",
            "server.js",
            "--yes",
        ],
    );
    assert_eq!(
        added["server"]["args"],
        serde_json::json!(["-y", "server.js"])
    );
}
