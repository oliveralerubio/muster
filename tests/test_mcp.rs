//! Ticket #13: `muster mcp` — MCP stdio JSON-RPC 2.0 server (RED suite).
//!
//! Every test drives the compiled binary over stdio with newline-delimited
//! JSON-RPC 2.0 messages, mirroring the minimal MCP lifecycle in
//! `docs`/agent_formats: initialize -> notifications/initialized -> tools.

mod support;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use support::MusterEnv;

const PROTOCOL_VERSION: &str = "2025-06-18";
const EXPECTED_TOOLS: [&str; 7] = [
    "status",
    "doctor",
    "plan",
    "propose_change",
    "secret_list_meta",
    "secret_reveal",
    "provider_list",
];

/// A live `muster mcp` subprocess bound to an isolated machine environment.
struct McpServer {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
}

impl McpServer {
    /// Spawn `muster mcp` with the same isolation `MusterEnv` uses for the CLI.
    fn spawn(env: &MusterEnv) -> Self {
        let path = format!(
            "{}:{}",
            env.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_muster"));
        cmd.arg("mcp")
            .env("HOME", env.path())
            .env("XDG_CONFIG_HOME", env.path().join(".config"))
            .env("XDG_STATE_HOME", env.path().join(".local/state"))
            .env("XDG_DATA_HOME", env.path().join(".local/share"))
            .env("XDG_CACHE_HOME", env.path().join(".cache"))
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, _) in std::env::vars() {
            if k.starts_with("MUSTER_") {
                cmd.env_remove(&k);
            }
        }
        let mut child = cmd.spawn().expect("spawn muster mcp");
        let stdin = child.stdin.take().expect("mcp stdin");
        let stdout = child.stdout.take().expect("mcp stdout");
        Self {
            child,
            stdin,
            reader: BufReader::new(stdout),
        }
    }

    /// Send one JSON-RPC message (request or notification) as one line.
    fn send(&mut self, message: &Value) {
        let line = format!("{message}\n");
        self.stdin
            .write_all(line.as_bytes())
            .expect("write to muster mcp stdin");
        self.stdin.flush().expect("flush muster mcp stdin");
    }

    /// Read the next JSON-RPC response line from stdout.
    fn read_response(&mut self) -> Value {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .expect("read muster mcp stdout");
        assert!(n > 0, "muster mcp closed stdout before answering");
        serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("invalid JSON-RPC from mcp server: {e}: {line}"))
    }

    /// Full MCP handshake: initialize, read result, notify initialized.
    fn initialize(&mut self) -> Value {
        self.initialize_as("muster-test")
    }

    /// Handshake announcing `client` as `clientInfo.name` (maps to an agent).
    fn initialize_as(&mut self, client: &str) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": client, "version": "0.0.0"}
            }
        }));
        let result = self.read_response()["result"].clone();
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));
        result
    }

    /// `tools/call` returning the parsed text content of the response.
    fn call_tool(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }));
        self.read_response()["result"].clone()
    }

    /// Close stdin (EOF) and wait for a clean exit.
    fn shutdown(mut self) -> std::process::ExitStatus {
        drop(self.stdin);
        self.child.wait().expect("wait for muster mcp")
    }
}

/// Extract the concatenated `text` of an MCP tool result content array.
fn tool_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .expect("tool result must have content array")
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse tool text as a muster JSON envelope (the parity contract).
fn envelope_of(result: &Value) -> Value {
    let text = tool_text(result);
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("tool text must be a muster JSON envelope: {e}: {text}"))
}

fn init_machine(env: &MusterEnv) {
    env.fake_exe("codex", "echo 'codex-cli 1.4.2'");
    env.fake_exe("claude", "echo '2.1.0 (Claude Code)'");
    env.fake_exe("opencode", "echo '0.5.0'");
    let (output, envelope) = env.run_json(&["--format", "json", "--yes", "init"]);
    assert!(
        output.status.success() && envelope.ok(),
        "machine init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// 1. Lifecycle: initialize, notifications, tools/list
// ---------------------------------------------------------------------------

#[test]
fn initialize_returns_protocol_version_capabilities_and_server_info() {
    let env = MusterEnv::new();
    let mut server = McpServer::spawn(&env);
    let result = server.initialize();
    assert_eq!(
        result["protocolVersion"].as_str(),
        Some(PROTOCOL_VERSION),
        "server must answer with the negotiated protocol version: {result}"
    );
    assert!(
        result["serverInfo"]["name"].is_string(),
        "serverInfo.name required: {result}"
    );
    assert!(
        !result["capabilities"]["tools"].is_null(),
        "server must advertise the tools capability: {result}"
    );
    let status = server.shutdown();
    assert!(status.success(), "clean handshake must exit 0: {status}");
}

#[test]
fn notifications_initialized_is_acknowledged_without_response() {
    let env = MusterEnv::new();
    let mut server = McpServer::spawn(&env);
    server.initialize();
    // A notification must not produce a JSON-RPC response: the next thing on
    // stdout must be the answer to the *following* request.
    server.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let response = server.read_response();
    assert_eq!(response["id"].as_i64(), Some(2));
    assert!(
        response["result"].is_object(),
        "tools/list result: {response}"
    );
    server.shutdown();
}

#[test]
fn tools_list_exposes_the_muster_tools() {
    let env = MusterEnv::new();
    let mut server = McpServer::spawn(&env);
    server.initialize();
    server.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let result = server.read_response()["result"].clone();
    let names: Vec<&str> = result["tools"]
        .as_array()
        .expect("tools/list returns an array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();
    for expected in EXPECTED_TOOLS {
        assert!(
            names.contains(&expected),
            "tools/list must expose `{expected}`; got {names:?}"
        );
    }
    for tool in result["tools"].as_array().unwrap() {
        assert_eq!(
            tool["inputSchema"]["type"].as_str(),
            Some("object"),
            "every tool needs an object inputSchema: {tool}"
        );
    }
    server.shutdown();
}

#[test]
fn ping_unknown_methods_and_malformed_lines_follow_json_rpc() {
    let env = MusterEnv::new();
    let mut server = McpServer::spawn(&env);
    server.initialize();

    server.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
    let pong = server.read_response();
    assert_eq!(pong["id"].as_i64(), Some(2));
    assert_eq!(pong["result"], json!({}), "ping answers an empty result");

    server.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}));
    let unknown = server.read_response();
    assert_eq!(unknown["id"].as_i64(), Some(3));
    assert_eq!(unknown["error"]["code"].as_i64(), Some(-32601), "{unknown}");

    server.stdin.write_all(b"{not json\n").unwrap();
    server.stdin.flush().unwrap();
    let malformed = server.read_response();
    assert_eq!(
        malformed["error"]["code"].as_i64(),
        Some(-32700),
        "{malformed}"
    );
    assert!(malformed["id"].is_null(), "{malformed}");

    // The server keeps serving after an error.
    server.send(&json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}));
    assert_eq!(server.read_response()["id"].as_i64(), Some(4));
    assert!(server.shutdown().success());
}

#[test]
fn initialize_with_an_unsupported_version_answers_the_latest_supported_one() {
    let env = MusterEnv::new();
    let mut server = McpServer::spawn(&env);
    server.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"protocolVersion": "1999-01-01", "capabilities": {}}
    }));
    let result = server.read_response()["result"].clone();
    assert_eq!(result["protocolVersion"].as_str(), Some(PROTOCOL_VERSION));
    server.shutdown();
}

// ---------------------------------------------------------------------------
// 2. Parity: MCP tool result == CLI `--format json` envelope
// ---------------------------------------------------------------------------

#[test]
fn mcp_status_matches_cli_status_json_envelope() {
    let env = MusterEnv::new();
    init_machine(&env);

    let (_, cli_envelope) = env.run_json(&["--format", "json", "status"]);
    let cli = cli_envelope.raw.clone();
    assert!(cli["ok"].as_bool().unwrap_or(false), "cli status: {cli}");

    let mut server = McpServer::spawn(&env);
    server.initialize();
    let result = server.call_tool(2, "status", json!({}));
    let mcp = envelope_of(&result);
    assert_eq!(
        mcp, cli,
        "MCP `status` must return the same envelope as `muster --format json status`"
    );
    server.shutdown();
}

#[test]
fn mcp_doctor_matches_cli_doctor_json_envelope() {
    let env = MusterEnv::new();
    init_machine(&env);

    let (_, cli_envelope) = env.run_json(&["--format", "json", "doctor"]);
    let cli = cli_envelope.raw.clone();
    assert!(cli["ok"].as_bool().unwrap_or(false), "cli doctor: {cli}");

    let mut server = McpServer::spawn(&env);
    server.initialize();
    let result = server.call_tool(2, "doctor", json!({}));
    let mcp = envelope_of(&result);
    assert_eq!(
        mcp, cli,
        "MCP `doctor` must return the same envelope as `muster --format json doctor`"
    );
    server.shutdown();
}

// ---------------------------------------------------------------------------
// 3. Propose change: plan is produced, nothing applied without approval
// ---------------------------------------------------------------------------

#[test]
fn every_read_only_tool_matches_its_cli_command() {
    let env = MusterEnv::new();
    init_machine(&env);
    seed_secret(&env, "ci/deploy_token", REVEAL_VALUE);
    let manifest = format!(
        "{}\n[mcp_server]\nreveal = [\"native://ci/deploy_token\"]\n\n\
         [secrets.\"native://ci/deploy_token\"]\nallow = [\"claude\"]\n",
        manifest_text(&env)
    );
    std::fs::write(env.config_dir().join("muster.toml"), manifest).unwrap();

    let cases: [(&str, Value, &[&str]); 3] = [
        ("plan", json!({}), &["--format", "json", "plan"]),
        (
            "secret_list_meta",
            json!({}),
            &["--format", "json", "secret", "list"],
        ),
        (
            "secret_reveal",
            json!({"reference": "native://ci/deploy_token"}),
            &[
                "--format",
                "json",
                "--yes",
                "secret",
                "get",
                "native://ci/deploy_token",
                "--reveal",
            ],
        ),
    ];
    let mut server = McpServer::spawn(&env);
    server.initialize_as("claude-code");
    for (id, (tool, arguments, cli_args)) in cases.into_iter().enumerate() {
        let (_, cli) = env.run_json(cli_args);
        let mcp = envelope_of(&server.call_tool(id as i64 + 2, tool, arguments));
        assert_eq!(
            mcp, cli.raw,
            "MCP `{tool}` differs from `muster {cli_args:?}`"
        );
    }
    server.shutdown();
}

fn manifest_text(env: &MusterEnv) -> String {
    std::fs::read_to_string(env.config_dir().join("muster.toml")).expect("read muster.toml")
}

/// A proposed manifest that adds a stdio MCP server for every agent.
fn proposed_manifest(env: &MusterEnv) -> String {
    format!(
        "{}\n[mcp.demo]\ncommand = \"demo-mcp-server\"\n",
        manifest_text(env)
    )
}

#[test]
fn propose_change_returns_the_plan_of_the_proposal_and_applies_nothing() {
    let env = MusterEnv::new();
    init_machine(&env);
    let before = manifest_text(&env);

    let mut server = McpServer::spawn(&env);
    server.initialize();
    let result = server.call_tool(
        2,
        "propose_change",
        json!({"path": "muster.toml", "contents": proposed_manifest(&env), "message": "add demo MCP server"}),
    );
    let envelope = envelope_of(&result);
    server.shutdown();

    assert_eq!(result["isError"], json!(false), "{envelope}");
    assert_eq!(envelope["command"], json!("proposal.create"));
    let proposal = &envelope["result"];
    let id = proposal["id"].as_str().expect("proposal id");
    assert_eq!(proposal["plan"]["applied"], json!(false), "{proposal}");
    // The plan is computed from the proposed config: the new server needs approval.
    let approvals = proposal["plan"]["approvals"].to_string();
    assert!(
        approvals.contains("demo"),
        "plan must show the proposal: {proposal}"
    );

    // Nothing applied: the live manifest is untouched and a real plan knows
    // nothing about the proposed server.
    assert_eq!(
        manifest_text(&env),
        before,
        "a proposal must not edit muster.toml"
    );
    let (_, plan) = env.run_json(&["--format", "json", "plan"]);
    assert!(
        !plan.raw.to_string().contains("demo-mcp-server"),
        "{}",
        plan.raw
    );
    assert!(env
        .config_dir()
        .join("proposals")
        .join(id)
        .join("files/muster.toml")
        .is_file());
}

#[test]
fn proposal_is_applied_only_after_explicit_acceptance() {
    let env = MusterEnv::new();
    init_machine(&env);
    let mut server = McpServer::spawn(&env);
    server.initialize();
    let result = server.call_tool(
        2,
        "propose_change",
        json!({"path": "muster.toml", "contents": proposed_manifest(&env)}),
    );
    server.shutdown();
    let id = envelope_of(&result)["result"]["id"]
        .as_str()
        .expect("proposal id")
        .to_string();

    // No TTY and no --yes: refused with a usage error, nothing changes.
    let (refused, _) = env.run_json(&["--format", "json", "proposal", "accept", &id]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(!manifest_text(&env).contains("demo-mcp-server"));

    let (accepted, envelope) =
        env.run_json(&["--format", "json", "--yes", "proposal", "accept", &id]);
    assert!(accepted.status.success(), "{}", envelope.raw);
    assert!(manifest_text(&env).contains("demo-mcp-server"));
    assert!(!env.config_dir().join("proposals").join(&id).exists());
    // Applying to the machine still goes through plan + sync approval.
    let (_, plan) = env.run_json(&["--format", "json", "plan"]);
    assert!(
        plan.raw["result"]["approvals"].to_string().contains("demo"),
        "{}",
        plan.raw
    );
}

#[test]
fn propose_change_rejects_paths_outside_the_reviewable_config() {
    let env = MusterEnv::new();
    init_machine(&env);
    let mut server = McpServer::spawn(&env);
    server.initialize();
    let mut id = 2;
    for path in [
        "../escape",
        "/etc/passwd",
        "vault/ci.age",
        ".git/config",
        "proposals/x",
    ] {
        let result = server.call_tool(id, "propose_change", json!({"path": path, "contents": "x"}));
        id += 1;
        let envelope = envelope_of(&result);
        assert_eq!(result["isError"], json!(true), "{path}: {envelope}");
        assert_eq!(envelope["diagnostics"][0]["code"], json!("usage"), "{path}");
    }
    // An invalid manifest is reported as invalid config, not stored.
    let result = server.call_tool(
        id,
        "propose_change",
        json!({"path": "muster.toml", "contents": "bogus = 1"}),
    );
    assert_eq!(
        envelope_of(&result)["diagnostics"][0]["code"],
        json!("invalid_config")
    );
    server.shutdown();
    assert!(!env.config_dir().join("proposals").exists());
}

#[test]
fn mutating_parameters_and_unknown_arguments_are_not_accepted() {
    let env = MusterEnv::new();
    init_machine(&env);
    let mut server = McpServer::spawn(&env);
    server.initialize();
    server.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = server.read_response()["result"]["tools"].clone();
    let doctor = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "doctor")
        .expect("doctor tool");
    assert!(
        doctor["inputSchema"]["properties"].get("fix").is_none(),
        "{doctor}"
    );
    let names: Vec<_> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    for forbidden in ["sync", "init", "secret_add", "rollback", "proposal_accept"] {
        assert!(
            !names.contains(&json!(forbidden)),
            "{forbidden} exposed: {names:?}"
        );
    }

    server.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "doctor", "arguments": {"fix": true}}}));
    let response = server.read_response();
    assert_eq!(
        response["error"]["code"].as_i64(),
        Some(-32602),
        "{response}"
    );

    server.send(&json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "plan", "arguments": {"project": 7}}}));
    let response = server.read_response();
    assert_eq!(
        response["error"]["code"].as_i64(),
        Some(-32602),
        "{response}"
    );

    server.send(&json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": {"name": "sync", "arguments": {}}}));
    let response = server.read_response();
    assert_eq!(
        response["error"]["code"].as_i64(),
        Some(-32602),
        "{response}"
    );
    server.shutdown();
}

// ---------------------------------------------------------------------------
// 4. Secret hygiene: values never appear in any MCP response
// ---------------------------------------------------------------------------

#[test]
fn secret_values_never_appear_in_mcp_responses() {
    let env = MusterEnv::new();
    init_machine(&env);
    const SECRET_VALUE: &str = "muster-red-SUPERSECRET-value-9f2a";

    // Seed a secret through the CLI (explicit per-secret permission is absent,
    // so no MCP response may ever carry the plaintext).
    let mut add_cmd = env.run(&[
        "--format",
        "json",
        "--yes",
        "secret",
        "add",
        "ci/deploy_token",
        "--value-stdin",
    ]);
    add_cmd.write_stdin(SECRET_VALUE);
    let (add_output, seeded) = support::json(add_cmd);
    assert!(
        add_output.status.success() && seeded.ok(),
        "seeding the test secret failed: {}",
        seeded.raw
    );

    let mut server = McpServer::spawn(&env);
    server.initialize();

    // Listing secrets (metadata tool) must not reveal the value...
    let listing = server.call_tool(2, "secret_list_meta", json!({}));
    let listing_text = tool_text(&listing);
    assert!(
        !listing_text.contains(SECRET_VALUE),
        "secret_list_meta leaked the secret plaintext: {listing_text}"
    );

    // ...and neither may any other tool response.
    for (id, tool) in (3..).zip(["status", "doctor", "plan"]) {
        let result = server.call_tool(id, tool, json!({}));
        let text = tool_text(&result);
        assert!(
            !text.contains(SECRET_VALUE),
            "MCP tool `{tool}` leaked the secret plaintext: {text}"
        );
    }
    server.shutdown();
}

const REVEAL_VALUE: &str = "muster-reveal-ALLOWLISTED-7c1d";

fn seed_secret(env: &MusterEnv, path: &str, value: &str) {
    let mut cmd = env.run(&["--format", "json", "secret", "add", path, "--value-stdin"]);
    cmd.write_stdin(value);
    let (output, envelope) = support::json(cmd);
    assert!(output.status.success(), "seed {path}: {}", envelope.raw);
}

#[test]
fn secret_reveal_is_denied_without_an_explicit_per_secret_allowance() {
    let env = MusterEnv::new();
    init_machine(&env);
    seed_secret(&env, "ci/deploy_token", REVEAL_VALUE);
    seed_secret(&env, "ci/other", "muster-OTHER-not-allowed-55");
    // Allowing a different secret does not allow this one.
    let manifest = format!(
        "{}\n[mcp_server]\nreveal = [\"native://ci/other-x\"]\n",
        manifest_text(&env)
    );
    std::fs::write(env.config_dir().join("muster.toml"), manifest).unwrap();

    let mut server = McpServer::spawn(&env);
    server.initialize();
    let result = server.call_tool(
        2,
        "secret_reveal",
        json!({"reference": "native://ci/deploy_token"}),
    );
    server.shutdown();
    let text = tool_text(&result);
    assert!(!text.contains(REVEAL_VALUE), "denied reveal leaked: {text}");
    assert_eq!(result["isError"], json!(true));
    let envelope = envelope_of(&result);
    assert_eq!(
        envelope["diagnostics"][0]["code"],
        json!("security"),
        "{envelope}"
    );
}

#[test]
fn secret_reveal_returns_only_the_explicitly_allowed_secret() {
    let env = MusterEnv::new();
    init_machine(&env);
    seed_secret(&env, "ci/deploy_token", REVEAL_VALUE);
    let manifest = format!(
        "{}\n[mcp_server]\nreveal = [\"native://ci/deploy_token\"]\n\n\
         [secrets.\"native://ci/deploy_token\"]\nallow = [\"claude\"]\n",
        manifest_text(&env)
    );
    std::fs::write(env.config_dir().join("muster.toml"), manifest).unwrap();

    let mut server = McpServer::spawn(&env);
    server.initialize_as("claude-code");
    let result = server.call_tool(
        2,
        "secret_reveal",
        json!({"reference": "native://ci/deploy_token"}),
    );
    let envelope = envelope_of(&result);
    assert_eq!(result["isError"], json!(false), "{envelope}");
    assert_eq!(envelope["result"], json!(REVEAL_VALUE));
    // The allowance never extends to listings or other tools.
    let listing = tool_text(&server.call_tool(3, "secret_list_meta", json!({})));
    assert!(!listing.contains(REVEAL_VALUE), "{listing}");
    server.shutdown();
}

/// Manifest listing the token for MCP reveal and granting it only to claude.
fn reveal_granted_to_claude(env: &MusterEnv) {
    let manifest = format!(
        "{}\n[mcp_server]\nreveal = [\"native://ci/deploy_token\"]\n\n\
         [secrets.\"native://ci/deploy_token\"]\nallow = [\"claude\"]\n",
        manifest_text(env)
    );
    std::fs::write(env.config_dir().join("muster.toml"), manifest).unwrap();
}

#[test]
fn secret_reveal_succeeds_for_a_client_in_the_secret_allow_list() {
    let env = MusterEnv::new();
    init_machine(&env);
    seed_secret(&env, "ci/deploy_token", REVEAL_VALUE);
    reveal_granted_to_claude(&env);

    let mut server = McpServer::spawn(&env);
    server.initialize_as("claude-code");
    let result = server.call_tool(
        2,
        "secret_reveal",
        json!({"reference": "native://ci/deploy_token"}),
    );
    server.shutdown();
    let envelope = envelope_of(&result);
    assert_eq!(result["isError"], json!(false), "{envelope}");
    assert_eq!(envelope["result"], json!(REVEAL_VALUE));
    let history =
        std::fs::read_to_string(env.path().join(".local/state/muster/history.jsonl")).unwrap();
    let reveals: Vec<Value> = history
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["action"] == "secret.reveal")
        .collect();
    assert_eq!(reveals.len(), 1);
    assert_eq!(reveals[0]["actor"], "claude");
    assert_eq!(reveals[0]["subject"], "native://ci/deploy_token");
    assert!(!history.contains(REVEAL_VALUE));
}

#[test]
fn secret_reveal_is_denied_to_a_client_outside_the_secret_allow_list() {
    let env = MusterEnv::new();
    init_machine(&env);
    seed_secret(&env, "ci/deploy_token", REVEAL_VALUE);
    reveal_granted_to_claude(&env);

    // Listed in [mcp_server] reveal, but codex is not in the secret's allow
    // list; an unidentified client is never granted either.
    for client in ["codex-mcp-client", "muster-test"] {
        let mut server = McpServer::spawn(&env);
        server.initialize_as(client);
        let result = server.call_tool(
            2,
            "secret_reveal",
            json!({"reference": "native://ci/deploy_token"}),
        );
        server.shutdown();
        let text = tool_text(&result);
        assert!(
            !text.contains(REVEAL_VALUE),
            "{client} reveal leaked: {text}"
        );
        assert_eq!(result["isError"], json!(true), "{client}: {text}");
        let envelope = envelope_of(&result);
        assert_eq!(
            envelope["diagnostics"][0]["code"],
            json!("security"),
            "{client}: {envelope}"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Exit handling: EOF or interrupt ends the server cleanly
// ---------------------------------------------------------------------------

#[test]
fn server_exits_cleanly_on_stdin_eof() {
    let env = MusterEnv::new();
    let status = McpServer::spawn(&env).shutdown();
    assert!(
        status.success(),
        "muster mcp must exit 0 on stdin EOF, got: {status}"
    );
}

#[test]
fn server_exits_with_130_on_sigint() {
    let env = MusterEnv::new();
    let mut server = McpServer::spawn(&env);
    // Give the server a moment to be up, then interrupt it.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let killed = Command::new("kill")
        .args(["-INT", &server.child.id().to_string()])
        .status()
        .expect("run kill -INT");
    assert!(killed.success(), "kill -INT failed: {killed}");
    let status = server.child.wait().expect("wait for interrupted mcp");
    assert_eq!(
        status.code(),
        Some(130),
        "SIGINT must map to exit code 130 per the muster exit-code contract"
    );
}

#[test]
fn secret_reveal_requires_a_lease_and_does_not_expose_approval_tools_or_agent_override() {
    let env = MusterEnv::new();
    init_machine(&env);
    seed_secret(&env, "ci/deploy_token", REVEAL_VALUE);
    reveal_granted_to_claude(&env);
    let path = env.config_dir().join("muster.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("approval = 'each'\n");
    std::fs::write(&path, text).unwrap();
    let mut server = McpServer::spawn(&env);
    server.initialize_as("claude-code");
    for id in [2, 3] {
        let result = server.call_tool(
            id,
            "secret_reveal",
            json!({"reference":"native://ci/deploy_token"}),
        );
        assert_eq!(result["isError"], true);
        let text = tool_text(&result);
        assert!(text.contains("secret_approval_required"));
        assert!(!text.contains(REVEAL_VALUE));
    }
    server.send(&json!({"jsonrpc":"2.0", "id":4, "method":"tools/list", "params":{}}));
    let tools = server.read_response();
    let tools = tools["result"]["tools"].as_array().unwrap();
    for forbidden in [
        "secret.approve",
        "secret.revoke",
        "secret.deny",
        "secret_approve",
        "secret_revoke",
        "secret_deny",
    ] {
        assert!(tools.iter().all(|t| t["name"] != forbidden));
    }
    let reveal = tools.iter().find(|t| t["name"] == "secret_reveal").unwrap();
    assert!(reveal["inputSchema"]["properties"].get("agent").is_none());
    server.shutdown();
    let data: Value = serde_json::from_str(
        &std::fs::read_to_string(env.path().join(".local/state/muster/secret-leases.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(data["requests"].as_array().unwrap().len(), 1);
    assert_eq!(data["requests"][0]["source"], "mcp");
}
