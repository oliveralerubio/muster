//! Property-based and adversarial tests for the validators hardened in
//! commit 302c590. See `docs/security/verification.md`.

use muster::commands::approve::validate_recorded_approvals;
use muster::installer::{approval_identity, Pin};
use muster::manifest::write_atomic as manifest_write_atomic;
use muster::mcp::client_agent;
use muster::providers::cli::validate_bws_project;
use muster::skills::lock::is_valid_name;
use muster::vault::write_atomic as vault_write_atomic;
use proptest::prelude::*;
use std::collections::BTreeSet;

fn pin(version: &str, url: &str, sha: &str) -> Pin {
    Pin {
        version: version.into(),
        url: Some(url.into()),
        sha256: Some(sha.into()),
        bin: None,
        artifacts: Default::default(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    // ---- is_valid_name -------------------------------------------------

    #[test]
    fn name_never_accepts_dangerous_input(s in any::<String>()) {
        if is_valid_name(&s) {
            prop_assert!(!s.is_empty());
            prop_assert!(!s.contains('/') && !s.contains('\\'));
            prop_assert!(!s.contains(".."));
            prop_assert!(!s.contains('\0'));
            prop_assert!(!s.chars().any(char::is_control));
            prop_assert!(!s.starts_with('.') && !s.ends_with('.'));
            prop_assert!(!s.starts_with(' ') && !s.ends_with(' '));
            prop_assert!(!s.starts_with('-'));
        }
    }

    #[test]
    fn name_rejects_any_string_with_a_forbidden_char(
        prefix in "[a-z0-9]{0,8}",
        bad in prop::sample::select(vec!['/', '\\', '\0', '.', ' ', '\n', '\t', '\u{7f}', 'A', '_', 'é']),
        suffix in "[a-z0-9]{0,8}",
    ) {
        let cand = format!("{prefix}{bad}{suffix}"); prop_assert!(!is_valid_name(&cand));
    }

    #[test]
    fn name_accepts_well_formed(s in "[a-z0-9][a-z0-9-]{0,63}") {
        prop_assert!(is_valid_name(&s));
    }

    // ---- validate_bws_project -----------------------------------------

    #[test]
    fn bws_rejects_flag_like_and_dirty_input(s in any::<String>()) {
        let bad = s.starts_with('-')
            || s.len() > 64
            || s.chars().any(|c| c.is_whitespace() || c.is_control());
        if bad {
            prop_assert!(validate_bws_project(&s).is_err());
        }
        if validate_bws_project(&s).is_ok() {
            prop_assert!(!s.starts_with('-') && s.len() <= 64);
            prop_assert!(!s.chars().any(|c| c.is_whitespace() || c.is_control()));
        }
    }

    #[test]
    fn bws_rejects_leading_dash(s in "-.*") {
        prop_assert!(validate_bws_project(&s).is_err());
    }

    #[test]
    fn bws_accepts_valid_ids(s in "[A-Za-z0-9][A-Za-z0-9_.:-]{0,63}") {
        prop_assert!(validate_bws_project(&s).is_ok());
    }

    #[test]
    fn bws_rejects_overlong(s in "[a-z0-9]{65,200}") {
        prop_assert!(validate_bws_project(&s).is_err());
    }

    // ---- mcp::client_agent --------------------------------------------

    #[test]
    fn client_agent_rejects_mutated_names(
        base in prop::sample::select(vec!["claude", "claude-code", "codex", "opencode", "antigravity", "agy"]),
        junk in "[a-z0-9-]{1,8}",
    ) {
        let cand = format!("{junk}{base}"); prop_assert!(client_agent(&cand).is_none());
        let cand = format!("{base}{junk}"); prop_assert!(client_agent(&cand).is_none());
        let cand = format!("{junk}{base}{junk}"); prop_assert!(client_agent(&cand).is_none());
        let cand = format!(" {base}"); prop_assert!(client_agent(&cand).is_none());
        let cand = format!("{base} "); prop_assert!(client_agent(&cand).is_none());
        let cand = format!("{base}\0"); prop_assert!(client_agent(&cand).is_none());
    }

    #[test]
    fn client_agent_only_known_names_match(s in any::<String>()) {
        const KNOWN: [&str; 6] = ["claude", "claude-code", "codex", "opencode", "antigravity", "agy"];
        let known = KNOWN.contains(&s.to_ascii_lowercase().as_str());
        prop_assert_eq!(client_agent(&s).is_some(), known);
    }

    // ---- installer::approval_identity ---------------------------------

    #[test]
    fn approval_identity_is_deterministic(
        v in "[0-9.]{1,8}", u in "https://[a-z]{1,10}/[a-z]{1,10}", h in "[0-9a-f]{64}",
    ) {
        let a = approval_identity("claude", &pin(&v, &u, &h)).unwrap();
        let b = approval_identity("claude", &pin(&v, &u, &h)).unwrap();
        prop_assert_eq!(a, b);
    }

    #[test]
    fn approval_identity_changes_with_any_component(
        v in "[0-9.]{1,8}", u in "https://[a-z]{1,10}/[a-z]{1,10}", h in "[0-9a-f]{64}",
        v2 in "[0-9.]{1,8}", u2 in "https://[a-z]{1,10}/[a-z]{1,10}", h2 in "[0-9a-f]{64}",
    ) {
        let base = approval_identity("claude", &pin(&v, &u, &h)).unwrap();
        if v != v2 {
            prop_assert_ne!(&base, &approval_identity("claude", &pin(&v2, &u, &h)).unwrap());
        }
        if u != u2 {
            prop_assert_ne!(&base, &approval_identity("claude", &pin(&v, &u2, &h)).unwrap());
        }
        if h != h2 {
            prop_assert_ne!(&base, &approval_identity("claude", &pin(&v, &u, &h2)).unwrap());
        }
        prop_assert_ne!(&base, &approval_identity("codex", &pin(&v, &u, &h)).unwrap());
    }

    // ---- write_atomic --------------------------------------------------

    #[test]
    fn manifest_write_atomic_round_trips_and_leaves_no_temp(
        first in prop::collection::vec(any::<u8>(), 0..512),
        second in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("muster.toml");
        manifest_write_atomic(&path, &first).unwrap();
        prop_assert_eq!(std::fs::read(&path).unwrap(), first);
        manifest_write_atomic(&path, &second).unwrap();
        prop_assert_eq!(std::fs::read(&path).unwrap(), second);
        prop_assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn vault_write_atomic_round_trips_and_leaves_no_temp(
        first in prop::collection::vec(any::<u8>(), 0..512),
        second in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.age");
        vault_write_atomic(&path, &first).unwrap();
        vault_write_atomic(&path, &second).unwrap();
        prop_assert_eq!(std::fs::read(&path).unwrap(), second);
        prop_assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    // ---- approvers -----------------------------------------------------

    #[test]
    fn approvals_from_non_admins_are_rejected(
        admins in prop::collection::btree_set("[a-z]{1,6}", 1..4),
        outsider in "[A-Z0-9]{1,6}",
    ) {
        // Outsiders (pending / revoked / non-admin) use a disjoint alphabet.
        let mut approvals: Vec<String> = admins.iter().cloned().collect();
        prop_assert!(validate_recorded_approvals(&admins, &approvals).is_ok());
        approvals.push(outsider);
        prop_assert!(validate_recorded_approvals(&admins, &approvals).is_err());
    }

    #[test]
    fn duplicate_approvals_are_rejected(admins in prop::collection::btree_set("[a-z]{1,6}", 1..4)) {
        let first = admins.iter().next().unwrap().clone();
        let approvals = vec![first.clone(), first];
        prop_assert!(validate_recorded_approvals(&admins, &approvals).is_err());
    }
}

#[cfg(unix)]
#[test]
fn write_atomic_preserves_existing_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("muster.toml");
    std::fs::write(&path, b"old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    manifest_write_atomic(&path, b"new").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
}

#[cfg(unix)]
#[test]
fn vault_write_atomic_creates_private_files() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secret.age");
    vault_write_atomic(&path, b"x").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn write_atomic_failure_leaves_original_and_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    // Target is a directory: rename over it fails.
    let target = dir.path().join("target");
    std::fs::create_dir(&target).unwrap();
    assert!(manifest_write_atomic(&target, b"x").is_err());
    assert!(vault_write_atomic(&target, b"x").is_err());
    assert!(target.is_dir());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn approval_identity_is_case_insensitive_on_sha_only() {
    let lower = approval_identity("claude", &pin("1", "https://x/y", &"ab".repeat(32))).unwrap();
    let upper = approval_identity("claude", &pin("1", "https://x/y", &"AB".repeat(32))).unwrap();
    assert_eq!(lower, upper);
}

#[test]
fn revoked_or_pending_members_are_not_in_the_admin_set() {
    let active: BTreeSet<String> = ["alice".to_string()].into();
    for outsider in ["revoked-admin", "pending-admin", "plain-member"] {
        assert!(validate_recorded_approvals(&active, &[outsider.to_string()]).is_err());
    }
}
