//! `muster-cloud-ops backup verify-manifest`: the one manifest validator the
//! offsite copy uses. Exit 0 valid, 65 invalid; needs no configuration and no
//! database.

use std::process::{Command, Output};

const ID: &str = "20261118T000000Z-bbbbbbbb";

fn manifest() -> serde_json::Value {
    serde_json::json!({
        "format": "muster-backup", "version": 2, "backup_id": ID,
        "op_id": "11111111-2222-3333-4444-555555555555",
        "snapshot_at": "2026-11-18T00:00:00Z", "created_at": "2026-11-18T00:00:05Z",
        "encryption": {"scheme": "age-x25519", "recipient_sha256": "0".repeat(64)},
        "normal_expires_at": "2026-12-18T00:00:00Z", "tenants": [],
        "artifacts": [
            {"name": "db.dump.age", "kind": "db", "bytes": 3, "sha256": "a".repeat(64)},
            {"name": "git.tar.age", "kind": "git", "bytes": 3, "sha256": "b".repeat(64)},
        ],
    })
}

fn run(m: &serde_json::Value, extra: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("manifest.json");
    std::fs::write(&file, m.to_string()).unwrap();
    Command::new(env!("CARGO_BIN_EXE_muster-cloud-ops"))
        .env_clear()
        .args(["backup", "verify-manifest"])
        .arg(&file)
        .args(extra)
        .output()
        .expect("run muster-cloud-ops")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("exit code")
}

#[test]
fn a_valid_manifest_exits_0() {
    assert_eq!(code(&run(&manifest(), &[])), 0);
    assert_eq!(code(&run(&manifest(), &["--backup-id", ID])), 0);
}

#[test]
fn an_invalid_manifest_exits_65() {
    for (field, value) in [
        ("op_id", serde_json::json!("not-a-uuid")),
        ("version", serde_json::json!(3)),
    ] {
        let mut m = manifest();
        m[field] = value;
        assert_eq!(code(&run(&m, &[])), 65, "{field}");
    }
    let mut m = manifest();
    m["artifacts"][1]["kind"] = serde_json::json!("extra");
    assert_eq!(code(&run(&m, &[])), 65, "unknown artifact kind");
    assert_eq!(
        code(&run(
            &manifest(),
            &["--backup-id", "20261101T000000Z-aaaaaaaa"]
        )),
        65
    );
}

#[test]
fn a_missing_file_is_not_a_verdict() {
    let out = Command::new(env!("CARGO_BIN_EXE_muster-cloud-ops"))
        .env_clear()
        .args(["backup", "verify-manifest", "/nonexistent/manifest.json"])
        .output()
        .unwrap();
    assert_eq!(code(&out), 74);
}
