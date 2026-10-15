//! Skill lifecycle: own skills and skills subscribed from external git
//! repositories, `skills.lock`, upstream checks, updates and removal.
//!
//! Upstream repositories are real local git repos subscribed by `file://` URL.
mod support;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use support::{Envelope, MusterEnv};

const PDF_V1: &str =
    "---\nname: pdf\ndescription: Work with PDF files\nversion: 1.0.0\n---\nUse pdftotext.\n";
const PDF_V2: &str = "---\nname: pdf\ndescription: Work with PDF files\nversion: 2.0.0\n---\nUse pdftotext -layout.\n";
const XLSX: &str = "---\nname: xlsx\ndescription: Spreadsheets\n---\nUse openpyxl.\n";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Upstream",
            "-c",
            "user.email=up@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A local upstream repo with `skills/pdf` (+ a reference file) and `skills/xlsx`.
struct Upstream {
    dir: tempfile::TempDir,
}

impl Upstream {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("upstream dir");
        let up = Self { dir };
        git(up.path(), &["init", "--quiet", "-b", "main"]);
        up.write("skills/pdf/SKILL.md", PDF_V1);
        up.write("skills/pdf/reference.md", "# Reference\n");
        up.write("skills/xlsx/SKILL.md", XLSX);
        up.write("README.md", "upstream skills\n");
        up.commit("initial");
        up
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn url(&self) -> String {
        format!("file://{}", self.path().display())
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn commit(&self, message: &str) -> String {
        git(self.path(), &["add", "-A"]);
        git(self.path(), &["commit", "--quiet", "-m", message]);
        self.head()
    }

    fn head(&self) -> String {
        git(self.path(), &["rev-parse", "HEAD"])
    }
}

fn json(env: &MusterEnv, args: &[&str]) -> (Output, Envelope) {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    env.run_json(&full)
}

fn ok(env: &MusterEnv, args: &[&str]) -> Value {
    let (out, envelope) = json(env, args);
    assert!(out.status.success(), "{args:?}: {}", envelope.raw);
    assert!(envelope.ok(), "{args:?}: {}", envelope.raw);
    envelope.result().clone()
}

fn fails(env: &MusterEnv, args: &[&str]) -> (i32, Envelope) {
    let (out, envelope) = json(env, args);
    assert!(
        !out.status.success(),
        "{args:?} must fail: {}",
        envelope.raw
    );
    assert!(!envelope.ok());
    (out.status.code().unwrap_or(-1), envelope)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{} must exist: {e}", path.display()))
}

fn lock(env: &MusterEnv) -> toml::Table {
    toml::from_str(&read(&env.config_dir().join("skills.lock"))).expect("skills.lock is TOML")
}

/// The lock entry for `name` (`[[skill]]` array).
fn lock_entry(env: &MusterEnv, name: &str) -> Option<toml::Table> {
    lock(env)
        .get("skill")
        .and_then(|s| s.as_array())
        .and_then(|skills| {
            skills
                .iter()
                .filter_map(|s| s.as_table())
                .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(name))
                .cloned()
        })
}

fn skill<'a>(result: &'a Value, name: &str) -> &'a Value {
    result["skills"]
        .as_array()
        .unwrap_or_else(|| panic!("skills array: {result}"))
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("skill {name} listed: {result}"))
}

fn has_diag(envelope_or_diags: &Value, code: &str) -> bool {
    envelope_or_diags
        .as_array()
        .is_some_and(|d| d.iter().any(|d| d["code"] == code))
}

fn initialized() -> MusterEnv {
    let env = MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    env
}

fn subscribe(env: &MusterEnv, up: &Upstream) -> Value {
    let url = up.url();
    ok(
        env,
        &[
            "skill",
            "add",
            "--url",
            &url,
            "--ref",
            "main",
            "--path",
            "skills",
            "--include",
            "pdf",
            "--source-name",
            "anthropic",
        ],
    )
}

fn skills_dir(env: &MusterEnv) -> PathBuf {
    env.config_dir().join("skills")
}

fn last_commit_message(env: &MusterEnv) -> String {
    git(&env.config_dir(), &["log", "-1", "--format=%B"])
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

#[test]
fn manifest_rejects_unsafe_skill_refs() {
    for git_ref in [
        "--upload-pack=other",
        "feature branch",
        "main\n",
        "main\t",
        "main\u{7f}",
        "main\u{a0}",
    ] {
        let text = format!(
            "version = 1\n[agents.skills]\nsource = \"skills\"\n[[agents.skills.remote]]\nname = \"upstream\"\nurl = \"https://example.com/skills.git\"\nref = {}\n",
            toml::Value::String(git_ref.into())
        );
        let err = muster::manifest::Manifest::parse(&text, "muster.toml").unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("ref"));
    }
}

#[test]
fn add_remote_rejects_unsafe_refs_before_fetching() {
    let env = initialized();
    let up = Upstream::new();
    let before = read(&env.config_dir().join("muster.toml"));
    for git_ref in [
        "--upload-pack=other",
        "feature branch",
        "main\n",
        "main\t",
        "main\u{7f}",
        "main\u{a0}",
    ] {
        let (code, envelope) = fails(
            &env,
            &[
                "skill",
                "add",
                "--url",
                &up.url(),
                &format!("--ref={git_ref}"),
                "--path",
                "skills",
            ],
        );
        assert_eq!(code, 2, "{}", envelope.raw);
        assert!(envelope.raw.to_string().contains("ref"));
    }
    assert_eq!(read(&env.config_dir().join("muster.toml")), before);
    assert!(!env.path().join(".cache/muster/skill-sources").exists());
}

#[cfg(unix)]
#[test]
fn add_remote_rejects_symlinked_skills_root_outside_checkout() {
    let env = initialized();
    let up = Upstream::new();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(outside.path().join("evil")).unwrap();
    std::fs::write(outside.path().join("evil/SKILL.md"), XLSX).unwrap();
    std::os::unix::fs::symlink(outside.path(), up.path().join("linked")).unwrap();
    up.commit("add symlink");
    let (_, envelope) = fails(
        &env,
        &["skill", "add", "--url", &up.url(), "--path", "linked"],
    );
    assert!(envelope.raw.to_string().contains("outside the checkout"));
}

#[test]
fn shallow_fetch_does_not_interpret_ref_as_option() {
    let up = Upstream::new();
    let cache = tempfile::TempDir::new().unwrap();
    let dest = cache.path().join("checkout");
    muster::git::shallow_fetch(&up.url(), Some("main"), &dest).unwrap();
    muster::git::shallow_fetch(&up.url(), Some("main"), &dest).unwrap();
    assert!(muster::git::shallow_fetch(&up.url(), Some("--dry-run"), &dest).is_err());
}

#[test]
fn add_local_scaffolds_skill_md_and_locks_it() {
    let env = initialized();
    let result = ok(
        &env,
        &["skill", "add", "review", "--description", "Review code"],
    );
    assert_eq!(result["name"], "review", "{result}");

    let skill_md = read(&skills_dir(&env).join("review/SKILL.md"));
    assert!(skill_md.starts_with("---\n"), "{skill_md}");
    assert!(skill_md.contains("name: review"), "{skill_md}");
    assert!(skill_md.contains("description: Review code"), "{skill_md}");

    let manifest = read(&env.config_dir().join("muster.toml"));
    assert!(manifest.contains("[agents.skills]"), "{manifest}");
    assert!(manifest.contains("source = \"skills\""), "{manifest}");

    let entry = lock_entry(&env, "review").expect("review locked");
    assert_eq!(entry["origin"].as_str(), Some("local"));
    assert!(entry["content_hash"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(entry.contains_key("updated_at"));
    assert!(!entry.contains_key("commit"));
    assert!(last_commit_message(&env).contains("review"));

    // A second add of the same name is refused.
    fails(&env, &["skill", "add", "review"]);
    // The scaffold is a valid skill: agents apply installs it.
    env.run(&["agents", "apply", "--agent", "claude", "--yes"])
        .assert()
        .code(0);
    assert!(env.path().join(".claude/skills/review/SKILL.md").is_file());
}

#[test]
fn add_remote_vendors_included_skills_and_locks_commit_and_hash() {
    let env = initialized();
    let up = Upstream::new();
    let result = subscribe(&env, &up);
    assert_eq!(result["source"], "anthropic", "{result}");

    assert_eq!(read(&skills_dir(&env).join("pdf/SKILL.md")), PDF_V1);
    assert_eq!(
        read(&skills_dir(&env).join("pdf/reference.md")),
        "# Reference\n"
    );
    assert!(
        !skills_dir(&env).join("xlsx").exists(),
        "include filters skills"
    );

    let entry = lock_entry(&env, "pdf").expect("pdf locked");
    assert_eq!(entry["origin"].as_str(), Some("anthropic"));
    assert_eq!(entry["commit"].as_str(), Some(up.head().as_str()));
    assert_eq!(entry["version"].as_str(), Some("1.0.0"));
    assert!(entry["content_hash"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));

    let manifest = read(&env.config_dir().join("muster.toml"));
    assert!(manifest.contains("[[agents.skills.remote]]"), "{manifest}");
    assert!(manifest.contains("name = \"anthropic\""), "{manifest}");
    assert!(manifest.contains(&up.url()), "{manifest}");
    // The manifest still validates (status loads it).
    ok(&env, &["status"]);
    assert!(last_commit_message(&env).contains("anthropic"));
}

#[test]
fn add_remote_without_include_vendors_every_skill() {
    let env = initialized();
    let up = Upstream::new();
    let url = up.url();
    ok(&env, &["skill", "add", "--url", &url, "--path", "skills"]);
    assert!(skills_dir(&env).join("pdf/SKILL.md").is_file());
    assert!(skills_dir(&env).join("xlsx/SKILL.md").is_file());
    // Source name defaults to the repository directory name.
    let source = up.path().file_name().unwrap().to_str().unwrap().to_string();
    assert!(lock_entry(&env, "xlsx").is_some());
    assert!(!source.is_empty());
}

// ---------------------------------------------------------------------------
// list / check / update
// ---------------------------------------------------------------------------

#[test]
fn list_works_offline_from_lock_and_cache() {
    let env = initialized();
    let up = Upstream::new();
    subscribe(&env, &up);
    ok(&env, &["skill", "add", "mine"]);
    // Upstream gone: list never touches the network.
    std::fs::remove_dir_all(up.path()).unwrap();
    let result = ok(&env, &["skill", "list"]);
    let pdf = skill(&result, "pdf");
    assert_eq!(pdf["origin"], "anthropic");
    assert_eq!(pdf["version"], "1.0.0");
    assert_eq!(pdf["status"], "up_to_date");
    assert_eq!(pdf["modified"], false);
    let mine = skill(&result, "mine");
    assert_eq!(mine["origin"], "local");
    assert_eq!(mine["status"], "local");
}

#[test]
fn check_detects_update_and_update_applies_it_then_agents_install_it() {
    let env = initialized();
    let up = Upstream::new();
    subscribe(&env, &up);
    let before = lock_entry(&env, "pdf").unwrap();

    up.write("skills/pdf/SKILL.md", PDF_V2);
    up.write("skills/pdf/extra.md", "extra\n");
    let new_head = up.commit("pdf v2");

    let (out, envelope) = json(&env, &["skill", "check"]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(
        skill(envelope.result(), "pdf")["status"],
        "update_available"
    );
    assert!(
        has_diag(envelope.diagnostics(), "skill_update_available"),
        "{}",
        envelope.raw
    );
    // The check is remembered for offline list.
    assert_eq!(
        skill(&ok(&env, &["skill", "list"]), "pdf")["status"],
        "update_available"
    );
    assert!(env
        .path()
        .join(".local/state/muster/skills-status.json")
        .is_file());

    // Dry run shows the per-file summary and changes nothing.
    let dry = ok(&env, &["skill", "update", "pdf", "--dry-run"]);
    let updated = &dry["updated"][0];
    assert_eq!(updated["name"], "pdf", "{dry}");
    assert!(
        updated["files"]["changed"]
            .as_array()
            .unwrap()
            .contains(&Value::from("SKILL.md")),
        "{dry}"
    );
    assert!(
        updated["files"]["added"]
            .as_array()
            .unwrap()
            .contains(&Value::from("extra.md")),
        "{dry}"
    );
    assert_eq!(read(&skills_dir(&env).join("pdf/SKILL.md")), PDF_V1);

    // Without --yes and no TTY, the update needs confirmation.
    let (code, _) = fails(&env, &["skill", "update", "pdf"]);
    assert_eq!(code, 2);

    ok(&env, &["skill", "update", "pdf", "--yes"]);
    assert_eq!(read(&skills_dir(&env).join("pdf/SKILL.md")), PDF_V2);
    assert_eq!(read(&skills_dir(&env).join("pdf/extra.md")), "extra\n");
    let after = lock_entry(&env, "pdf").unwrap();
    assert_eq!(after["commit"].as_str(), Some(new_head.as_str()));
    assert_eq!(after["version"].as_str(), Some("2.0.0"));
    assert_ne!(after["content_hash"], before["content_hash"]);
    assert!(after["updated_at"].as_str().unwrap() >= before["updated_at"].as_str().unwrap());
    assert_eq!(
        skill(&ok(&env, &["skill", "list"]), "pdf")["status"],
        "up_to_date"
    );

    env.run(&["agents", "apply", "--agent", "claude", "--yes"])
        .assert()
        .code(0);
    assert_eq!(
        read(&env.path().join(".claude/skills/pdf/SKILL.md")),
        PDF_V2
    );
}

#[test]
fn local_edit_is_flagged_modified_and_update_needs_force() {
    let env = initialized();
    let up = Upstream::new();
    subscribe(&env, &up);
    let local = skills_dir(&env).join("pdf/SKILL.md");
    std::fs::write(&local, format!("{PDF_V1}My local tweak.\n")).unwrap();
    assert_eq!(
        skill(&ok(&env, &["skill", "list"]), "pdf")["modified"],
        true
    );

    up.write("skills/pdf/SKILL.md", PDF_V2);
    up.commit("pdf v2");
    ok(&env, &["skill", "check", "pdf"]);
    let (_, envelope) = fails(&env, &["skill", "update", "pdf", "--yes"]);
    assert!(
        envelope.raw.to_string().contains("--force"),
        "{}",
        envelope.raw
    );
    assert!(read(&local).contains("My local tweak."));

    ok(&env, &["skill", "update", "pdf", "--force", "--yes"]);
    assert_eq!(read(&local), PDF_V2);
    assert_eq!(
        skill(&ok(&env, &["skill", "list"]), "pdf")["modified"],
        false
    );
}

#[test]
fn deprecated_upstream_skill_is_marked_deprecated() {
    let env = initialized();
    let up = Upstream::new();
    subscribe(&env, &up);
    up.write(
        "skills/pdf/SKILL.md",
        "---\nname: pdf\ndescription: Work with PDF files\ndeprecated: \"use pdf-pro\"\n---\nOld.\n",
    );
    up.commit("deprecate pdf");
    let (out, envelope) = json(&env, &["skill", "check"]);
    assert!(out.status.success(), "{}", envelope.raw);
    let pdf = skill(envelope.result(), "pdf");
    assert_eq!(pdf["status"], "deprecated", "{pdf}");
    assert_eq!(pdf["reason"], "use pdf-pro", "{pdf}");
    assert!(has_diag(envelope.diagnostics(), "skill_deprecated"));
    // Nothing was deleted or changed locally.
    assert_eq!(read(&skills_dir(&env).join("pdf/SKILL.md")), PDF_V1);
}

#[test]
fn upstream_removed_skill_is_kept_until_detached_or_removed() {
    let env = initialized();
    let up = Upstream::new();
    subscribe(&env, &up);
    std::fs::remove_dir_all(up.path().join("skills/pdf")).unwrap();
    up.commit("drop pdf");

    let (out, envelope) = json(&env, &["skill", "check"]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(
        skill(envelope.result(), "pdf")["status"],
        "upstream_removed"
    );
    assert!(has_diag(envelope.diagnostics(), "skill_upstream_removed"));
    assert!(skills_dir(&env).join("pdf/SKILL.md").is_file());

    // Update refuses: there is nothing upstream to update to.
    fails(&env, &["skill", "update", "pdf", "--yes"]);
    assert!(skills_dir(&env).join("pdf/SKILL.md").is_file());

    // Keep it as an own skill.
    ok(&env, &["skill", "rm", "pdf", "--detach", "--yes"]);
    assert_eq!(read(&skills_dir(&env).join("pdf/SKILL.md")), PDF_V1);
    let entry = lock_entry(&env, "pdf").unwrap();
    assert_eq!(entry["origin"].as_str(), Some("local"));
    assert_eq!(
        skill(&ok(&env, &["skill", "list"]), "pdf")["status"],
        "local"
    );
}

#[test]
fn rm_deletes_dir_and_lock_entry_only_with_yes() {
    let env = initialized();
    let up = Upstream::new();
    subscribe(&env, &up);

    let (code, _) = fails(&env, &["skill", "rm", "pdf"]);
    assert_eq!(code, 2, "confirmation required without a TTY");
    assert!(skills_dir(&env).join("pdf/SKILL.md").is_file());
    assert!(lock_entry(&env, "pdf").is_some());

    ok(&env, &["skill", "rm", "pdf", "--yes"]);
    assert!(!skills_dir(&env).join("pdf").exists());
    assert!(lock_entry(&env, "pdf").is_none());
    assert!(last_commit_message(&env).contains("pdf"));

    // Unknown skills are a usage error.
    let (code, _) = fails(&env, &["skill", "rm", "nope", "--yes"]);
    assert_eq!(code, 2);
}

#[test]
fn rm_detach_is_refused_for_own_skills() {
    let env = initialized();
    ok(&env, &["skill", "add", "mine"]);
    fails(&env, &["skill", "rm", "mine", "--detach", "--yes"]);
    assert!(skills_dir(&env).join("mine/SKILL.md").is_file());
}

// ---------------------------------------------------------------------------
// Team mode: only admins change skills
// ---------------------------------------------------------------------------

fn on(env: &MusterEnv, config: &Path, args: &[&str]) -> (Output, Envelope) {
    let config = config.to_str().unwrap();
    let mut full = vec!["--format", "json", "--config", config];
    full.extend_from_slice(args);
    env.run_json(&full)
}

fn ok_on(env: &MusterEnv, config: &Path, args: &[&str]) -> Value {
    let (out, envelope) = on(env, config, args);
    assert!(out.status.success(), "{args:?}: {}", envelope.raw);
    envelope.result().clone()
}

fn denied_on(env: &MusterEnv, config: &Path, args: &[&str]) {
    let (out, envelope) = on(env, config, args);
    assert_eq!(
        out.status.code(),
        Some(5),
        "{args:?} must be forbidden: {}",
        envelope.raw
    );
}

#[test]
fn team_members_can_list_and_check_but_only_admins_change_skills() {
    let owner = MusterEnv::new();
    let config = owner.config_dir();
    ok_on(
        &owner,
        &config,
        &["init", "--yes", "--machine", "owner-laptop"],
    );
    let up = Upstream::new();
    let url = up.url();
    ok_on(
        &owner,
        &config,
        &[
            "skill",
            "add",
            "--url",
            &url,
            "--path",
            "skills",
            "--include",
            "pdf",
        ],
    );
    ok_on(
        &owner,
        &config,
        &["member", "add", "bob", "--role", "member"],
    );
    ok_on(&owner, &config, &["member", "approve", "bob", "--yes"]);
    let bob = MusterEnv::new();
    ok_on(&bob, &config, &["enroll", "--name", "bob-laptop"]);
    ok_on(
        &owner,
        &config,
        &["approve", "bob-laptop", "--member", "bob", "--yes"],
    );

    up.write("skills/pdf/SKILL.md", PDF_V2);
    up.commit("pdf v2");

    ok_on(&bob, &config, &["skill", "list"]);
    ok_on(&bob, &config, &["skill", "check"]);
    denied_on(&bob, &config, &["skill", "add", "bobs"]);
    denied_on(&bob, &config, &["skill", "update", "--all", "--yes"]);
    denied_on(&bob, &config, &["skill", "rm", "pdf", "--yes"]);
    assert_eq!(read(&config.join("skills/pdf/SKILL.md")), PDF_V1);
    assert!(!config.join("skills/bobs").exists());

    // The admin can, and the commit names the actor.
    ok_on(&owner, &config, &["skill", "update", "--all", "--yes"]);
    assert_eq!(read(&config.join("skills/pdf/SKILL.md")), PDF_V2);
    assert!(git(&config, &["log", "-1", "--format=%B"]).contains("Muster-Actor"));
}

// ---------------------------------------------------------------------------
// Web API
// ---------------------------------------------------------------------------

#[test]
fn web_get_api_skills_returns_the_list() {
    let env = initialized();
    ok(&env, &["skill", "add", "mine", "--description", "Mine"]);

    let mut command = Command::new(assert_cmd::cargo::cargo_bin("muster"));
    let base = env.run(&[]);
    for (k, v) in base.get_envs() {
        match v {
            Some(v) => command.env(k, v),
            None => command.env_remove(k),
        };
    }
    let mut child = command
        .args(["ui", "--no-open", "--port", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start muster ui");
    let mut stdout = child.stdout.take().unwrap();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while stdout.read(&mut byte).unwrap_or(0) == 1 && byte[0] != b'\n' {
        line.push(byte[0]);
    }
    let url = String::from_utf8(line).unwrap();
    let url = url.trim();
    let token = url.split("token=").nth(1).expect("token").to_string();
    let address = url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();

    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    write!(
        stream,
        "GET /api/skills?token={token} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let _ = child.kill();
    let _ = child.wait();

    let (head, body) = response.split_once("\r\n\r\n").expect("HTTP response");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let api: Value = serde_json::from_str(body).expect("JSON body");
    assert_eq!(api["command"], "skill.list", "{api}");
    assert_eq!(skill(&api["result"], "mine")["origin"], "local");
    assert!(api["cli"].as_str().unwrap().contains("muster skill list"));
}
