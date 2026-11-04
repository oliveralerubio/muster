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

#[test]
fn missing_vendored_skill_can_be_restored_or_detached() {
    let env = initialized();
    let up = Upstream::new();
    subscribe(&env, &up);
    std::fs::remove_dir_all(skills_dir(&env).join("pdf")).unwrap();
    ok(&env, &["skill", "update", "pdf", "--force", "--yes"]);
    assert_eq!(read(&skills_dir(&env).join("pdf/SKILL.md")), PDF_V1);
    std::fs::remove_dir_all(skills_dir(&env).join("pdf")).unwrap();
    ok(&env, &["skill", "rm", "pdf", "--detach", "--yes"]);
    assert_eq!(
        lock_entry(&env, "pdf").unwrap()["origin"].as_str(),
        Some("local")
    );
}

#[test]
fn matching_upstream_refreshes_stale_pin_without_deleting_junk() {
    let env = initialized();
    let up = Upstream::new();
    subscribe(&env, &up);
    let dir = skills_dir(&env).join("pdf");
    std::fs::write(dir.join("note.bak-yolo"), "backup").unwrap();
    let mut lock = muster::skills::SkillsLock::load(&env.config_dir()).unwrap();
    let mut entry = lock.get("pdf").unwrap().clone();
    entry.content_hash = "sha256:old-filter-hash".into();
    lock.upsert(entry);
    lock.save(&env.config_dir()).unwrap();
    let result = ok(&env, &["skill", "update", "pdf", "--yes"]);
    assert_eq!(result["updated"].as_array().unwrap().len(), 1);
    let hash = muster::skills::content_hash(&muster::skills::read_skill_files(&dir).unwrap());
    assert_eq!(
        lock_entry(&env, "pdf").unwrap()["content_hash"].as_str(),
        Some(hash.as_str())
    );
    assert!(dir.join("note.bak-yolo").exists());
}

// ---------------------------------------------------------------------------
// Provenance-aware import and unattended auto-update
// ---------------------------------------------------------------------------

fn foo_md(version: &str) -> String {
    format!("---\nname: foo\ndescription: Foo skill\nversion: {version}\n---\nFoo {version}.\n")
}

fn bar_md(version: &str) -> String {
    format!("---\nname: bar\ndescription: Bar skill\nversion: {version}\n---\nBar {version}.\n")
}

/// Upstream with `skills/engineering/{foo,bar}` and `skills/misc/baz`.
fn engineering_upstream() -> Upstream {
    let up = Upstream::new();
    up.write("skills/engineering/foo/SKILL.md", &foo_md("1.0.0"));
    up.write("skills/engineering/bar/SKILL.md", &bar_md("1.0.0"));
    up.commit("engineering skills");
    up
}

/// Install `name` as an unmanaged skill in `~/.agents/skills`.
fn install_on_disk(env: &MusterEnv, name: &str, skill_md: &str) {
    let dir = env.path().join(".agents/skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), skill_md).unwrap();
}

/// Write `~/.agents/.skill-lock.json` the way `npx skills` does.
fn write_agents_lock(env: &MusterEnv, url: &str, skills: &[(&str, &str)]) {
    let mut entries = serde_json::Map::new();
    for (name, skill_path) in skills {
        entries.insert(
            (*name).into(),
            serde_json::json!({
                "source": "acme/skills",
                "sourceType": "github",
                "sourceUrl": url,
                "skillPath": skill_path,
                "skillFolderHash": "abc",
                "installedAt": "2026-01-01T00:00:00Z",
                "updatedAt": "2026-01-01T00:00:00Z",
                "someFutureField": 1,
            }),
        );
    }
    let text = serde_json::json!({ "version": 3, "skills": entries }).to_string();
    let path = env.path().join(".agents/.skill-lock.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn manifest_remotes(env: &MusterEnv) -> Vec<toml::Value> {
    let manifest: toml::Table =
        toml::from_str(&read(&env.config_dir().join("muster.toml"))).unwrap();
    manifest["agents"]["skills"]
        .get("remote")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default()
}

fn commit_count(env: &MusterEnv) -> usize {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(env.config_dir())
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .expect("git rev-list");
    if !out.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// A bare remote for the config repo, so `sync` pushes.
fn config_remote(env: &MusterEnv) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    let branch = git(&env.config_dir(), &["branch", "--show-current"]);
    git(dir.path(), &["init", "--bare", "--quiet", "-b", &branch]);
    ok(
        env,
        &["--yes", "sync", "--remote", dir.path().to_str().unwrap()],
    );
    dir
}

/// Env with `foo` installed from `up`, imported (subscribed) and auto-update on.
fn subscribed_foo(up: &Upstream, auto_update: bool) -> MusterEnv {
    let env = initialized();
    install_on_disk(&env, "foo", &foo_md("1.0.0"));
    write_agents_lock(
        &env,
        &up.url(),
        &[("foo", "skills/engineering/foo/SKILL.md")],
    );
    ok(&env, &["skill", "import", "foo", "--yes"]);
    if auto_update {
        ok(&env, &["skill", "auto-update", "on", "--yes"]);
    }
    env
}

#[test]
fn import_with_provenance_subscribes_instead_of_copying() {
    let up = engineering_upstream();
    let env = initialized();
    // The on-disk copy differs from upstream HEAD.
    install_on_disk(&env, "foo", &foo_md("0.9.0"));
    write_agents_lock(
        &env,
        &up.url(),
        &[("foo", "skills/engineering/foo/SKILL.md")],
    );

    let listed = ok(&env, &["skill", "list"]);
    let unmanaged = listed["unmanaged"].as_array().unwrap();
    let entry = unmanaged.iter().find(|u| u["name"] == "foo").unwrap();
    assert_eq!(entry["source"]["name"], "acme/skills");
    assert_eq!(entry["source"]["path"], "skills/engineering");

    let result = ok(&env, &["skill", "import", "foo", "--yes"]);
    assert_eq!(result["replaced_local_copy"], true, "{result}");
    assert_ne!(result["origin"], "local");
    assert_eq!(result["commit"], up.head());

    let remotes = manifest_remotes(&env);
    assert_eq!(remotes.len(), 1, "{remotes:?}");
    assert_eq!(remotes[0]["path"].as_str(), Some("skills/engineering"));
    assert_eq!(
        remotes[0]["include"].as_array().unwrap()[0].as_str(),
        Some("foo")
    );
    assert_eq!(remotes[0]["name"].as_str(), Some("acme-skills-engineering"));
    assert_eq!(
        read(&skills_dir(&env).join("foo/SKILL.md")),
        foo_md("1.0.0")
    );
    let locked = lock_entry(&env, "foo").unwrap();
    assert_eq!(locked["origin"].as_str(), Some("acme-skills-engineering"));
    assert_eq!(locked["commit"].as_str(), Some(up.head().as_str()));
    // Upstream files, not the on-disk copy, are what ships.
    assert_eq!(
        read(&env.path().join(".agents/skills/foo/SKILL.md")),
        foo_md("0.9.0"),
        "the original on-disk copy is untouched by import"
    );
}

#[test]
fn second_skill_from_same_url_and_path_merges_into_the_remote() {
    let up = engineering_upstream();
    let env = initialized();
    install_on_disk(&env, "foo", &foo_md("1.0.0"));
    install_on_disk(&env, "bar", &bar_md("1.0.0"));
    write_agents_lock(
        &env,
        &up.url(),
        &[
            ("foo", "skills/engineering/foo/SKILL.md"),
            ("bar", "skills/engineering/bar/SKILL.md"),
        ],
    );
    ok(&env, &["skill", "import", "foo", "--yes"]);
    ok(&env, &["skill", "import", "bar", "--yes"]);
    let remotes = manifest_remotes(&env);
    assert_eq!(remotes.len(), 1, "{remotes:?}");
    let include: Vec<&str> = remotes[0]["include"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(include, ["foo", "bar"]);
    assert_eq!(
        lock_entry(&env, "bar").unwrap()["origin"].as_str(),
        remotes[0]["name"].as_str()
    );
}

#[test]
fn import_fails_without_falling_back_when_upstream_is_unreachable() {
    let env = initialized();
    install_on_disk(&env, "foo", &foo_md("1.0.0"));
    write_agents_lock(
        &env,
        "file:///nonexistent/upstream-repo",
        &[("foo", "skills/engineering/foo/SKILL.md")],
    );
    let (_, envelope) = fails(&env, &["skill", "import", "foo", "--yes"]);
    assert!(
        envelope.raw.to_string().contains("cannot fetch upstream"),
        "{}",
        envelope.raw
    );
    let lock_path = env.config_dir().join("skills.lock");
    assert!(!lock_path.exists() || lock_entry(&env, "foo").is_none());
    assert!(!skills_dir(&env).join("foo").exists());
}

#[test]
fn import_all_mixes_subscribed_and_own_skills_in_one_commit() {
    let up = engineering_upstream();
    let env = initialized();
    install_on_disk(&env, "foo", &foo_md("1.0.0"));
    install_on_disk(&env, "bar", &bar_md("1.0.0"));
    install_on_disk(
        &env,
        "mine",
        "---\nname: mine\ndescription: Mine\n---\nOwn.\n",
    );
    install_on_disk(&env, "broken", "no frontmatter\n");
    write_agents_lock(
        &env,
        &up.url(),
        &[
            ("foo", "skills/engineering/foo/SKILL.md"),
            ("bar", "skills/engineering/bar/SKILL.md"),
        ],
    );
    let before = commit_count(&env);
    let result = ok(&env, &["skill", "import", "--all", "--yes"]);
    assert_eq!(commit_count(&env), before + 1, "one commit for the batch");
    let imported = result["imported"].as_array().unwrap();
    assert_eq!(imported.len(), 3, "{result}");
    assert_eq!(result["skipped"].as_array().unwrap().len(), 1, "{result}");
    assert_eq!(result["skipped"][0]["name"], "broken");
    assert_eq!(
        lock_entry(&env, "mine").unwrap()["origin"].as_str(),
        Some("local")
    );
    assert_ne!(
        lock_entry(&env, "foo").unwrap()["origin"].as_str(),
        Some("local")
    );
    // Both skills share one source: one remote, fetched once.
    let remotes = manifest_remotes(&env);
    assert_eq!(remotes.len(), 1, "{remotes:?}");
    assert_eq!(remotes[0]["include"].as_array().unwrap().len(), 2);
}

#[test]
fn sync_auto_updates_subscribed_skills_unattended_and_pushes() {
    let up = engineering_upstream();
    let env = subscribed_foo(&up, true);
    let remote = config_remote(&env);
    assert_eq!(
        ok(&env, &["skill", "list"])["auto_update"],
        true,
        "list reports the setting"
    );

    up.write("skills/engineering/foo/SKILL.md", &foo_md("1.1.0"));
    let new_head = up.commit("foo 1.1.0");

    // No --yes, stdin is not a terminal.
    let (out, envelope) = json(&env, &["sync"]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(
        envelope.result()["skills_updated"],
        serde_json::json!(["foo"]),
        "{}",
        envelope.raw
    );
    assert_eq!(
        read(&skills_dir(&env).join("foo/SKILL.md")),
        foo_md("1.1.0")
    );
    assert_eq!(
        lock_entry(&env, "foo").unwrap()["commit"].as_str(),
        Some(new_head.as_str())
    );
    let applied = env.path().join(".claude/skills/foo/SKILL.md");
    assert_eq!(read(&applied), foo_md("1.1.0"), "applied in the same run");
    assert_eq!(
        git(remote.path(), &["rev-parse", "HEAD"]),
        git(&env.config_dir(), &["rev-parse", "HEAD"]),
        "the update was pushed in the same run"
    );
}

#[test]
fn sync_leaves_skills_alone_when_auto_update_is_off() {
    let up = engineering_upstream();
    let env = subscribed_foo(&up, false);
    let _remote = config_remote(&env);
    up.write("skills/engineering/foo/SKILL.md", &foo_md("1.1.0"));
    up.commit("foo 1.1.0");
    let (out, envelope) = json(&env, &["sync"]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result()["skills_updated"], serde_json::json!([]));
    assert_eq!(
        read(&skills_dir(&env).join("foo/SKILL.md")),
        foo_md("1.0.0")
    );

    // Turning it off again removes the manifest key.
    ok(&env, &["skill", "auto-update", "on", "--yes"]);
    ok(&env, &["skill", "auto-update", "off"]);
    assert!(!read(&env.config_dir().join("muster.toml")).contains("auto_update"));
}

#[test]
fn sync_auto_update_never_overwrites_locally_modified_skills() {
    let up = engineering_upstream();
    let env = subscribed_foo(&up, true);
    let _remote = config_remote(&env);
    let edited = format!("{}local tweak\n", foo_md("1.0.0"));
    std::fs::write(skills_dir(&env).join("foo/SKILL.md"), &edited).unwrap();
    up.write("skills/engineering/foo/SKILL.md", &foo_md("1.1.0"));
    up.commit("foo 1.1.0");
    let (out, envelope) = json(&env, &["sync"]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(read(&skills_dir(&env).join("foo/SKILL.md")), edited);
    assert!(
        has_diag(envelope.diagnostics(), "skill_modified"),
        "{}",
        envelope.raw
    );
    assert_eq!(envelope.result()["skills_updated"], serde_json::json!([]));
}

#[test]
fn sync_auto_update_warns_but_succeeds_when_upstream_is_unreachable() {
    let up = engineering_upstream();
    let env = subscribed_foo(&up, true);
    let _remote = config_remote(&env);
    std::fs::remove_dir_all(up.path()).unwrap();
    let (out, envelope) = json(&env, &["sync"]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert!(
        has_diag(envelope.diagnostics(), "skill_source_unreachable"),
        "{}",
        envelope.raw
    );
    assert_eq!(
        read(&skills_dir(&env).join("foo/SKILL.md")),
        foo_md("1.0.0")
    );
}

#[test]
fn editing_an_own_skill_repins_it_but_subscribed_skills_are_not_pinned() {
    let up = engineering_upstream();
    let env = subscribed_foo(&up, false);
    ok(&env, &["skill", "add", "mine", "--description", "Mine"]);
    let before = lock_entry(&env, "mine").unwrap()["content_hash"]
        .as_str()
        .unwrap()
        .to_string();
    ok(
        &env,
        &[
            "skill",
            "file",
            "set",
            "mine",
            "--path",
            "SKILL.md",
            "--content=---\nname: mine\ndescription: Mine edited\n---\nNew body.\n",
            "--yes",
        ],
    );
    let dir = skills_dir(&env).join("mine");
    let hash = muster::skills::content_hash(&muster::skills::read_skill_files(&dir).unwrap());
    let after = lock_entry(&env, "mine").unwrap()["content_hash"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(before, after);
    assert_eq!(after, hash, "lock follows the edit");
    assert_eq!(git(&env.config_dir(), &["status", "--porcelain"]), "");
    // The edited own skill reaches the agent folders without a manual pin.
    ok(&env, &["sync", "--yes"]);
    assert!(read(&env.path().join(".claude/skills/mine/SKILL.md")).contains("Mine edited"));

    // Edits to a subscribed skill are not pinned.
    let pinned = lock_entry(&env, "foo").unwrap()["content_hash"]
        .as_str()
        .unwrap()
        .to_string();
    ok(
        &env,
        &[
            "skill",
            "file",
            "set",
            "foo",
            "--path",
            "SKILL.md",
            &format!("--content={}tweak\n", foo_md("1.0.0")),
            "--yes",
        ],
    );
    assert_eq!(
        lock_entry(&env, "foo").unwrap()["content_hash"].as_str(),
        Some(pinned.as_str())
    );
}
