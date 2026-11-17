//! Re-applies this machine's unpublished changes on top of the Cloud-accepted
//! state after a lost publication race (see `sync_transport::pull`).
//!
//! The working repository is already checked out at the accepted tip and
//! verified. The intent is the change `base..local` (the abandoned history,
//! kept under a backup ref by the caller), replayed operation by operation:
//!
//! - non-signed files: per-file three-way patch of `base..local`;
//! - `muster.toml`: three-way text merge, governance never changed by it;
//!   secret policies (`[secrets]`) merged per reference;
//! - the vault: every secret this machine added, rotated or removed, read in
//!   plaintext with this machine's identity from the abandoned commits and
//!   written again over the accepted ciphertext.
//!
//! A key both sides changed to different results is never guessed: it is
//! reported as a conflict and the accepted value stays. The caller re-seals
//! the result as the next epoch on top of the accepted state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::context::Context;
use crate::errors::MusterError;
use crate::fleet_state::{self, STATE_FILE};
use crate::identity::Identity;
use crate::manifest::{self, Manifest};
use crate::secrets::SecretValue;
use crate::vault::Vault;

/// The three commits of a forked history.
#[derive(Debug, Clone)]
pub(crate) struct Fork {
    /// `git merge-base local accepted`.
    pub base: String,
    /// This machine's abandoned HEAD.
    pub local: String,
    /// The verified Cloud tip, checked out in the working repository.
    pub accepted: String,
}

/// Something both sides changed differently, or that could not be replayed.
#[derive(Debug, Clone)]
pub(crate) struct Collision {
    /// What collided: `group/name` for a secret, `policy <reference>`,
    /// `file <path>`, `muster.toml`, ...
    pub subject: String,
    /// How to keep this machine's version (re-run the command).
    pub hint: String,
}

#[derive(Debug, Default)]
pub(crate) struct Replay {
    /// What was re-applied (secret keys, policies, files); never values.
    pub replayed: Vec<String>,
    pub collisions: Vec<Collision>,
}

impl Replay {
    fn collide(&mut self, subject: impl Into<String>, hint: impl Into<String>) {
        self.collisions.push(Collision {
            subject: subject.into(),
            hint: hint.into(),
        });
    }
}

fn op_err(what: &str) -> MusterError {
    MusterError::Operational(format!("cannot replay local changes: {what}"))
}

fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output, MusterError> {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|_| op_err("cannot run git"))
}

fn git_lines(dir: &Path, args: &[&str]) -> Result<Vec<String>, MusterError> {
    let out = git(dir, args)?;
    if !out.status.success() {
        return Err(op_err(&format!("git {}", args.first().unwrap_or(&""))));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Object id of `path` at `commit`, `None` when absent.
fn blob(dir: &Path, commit: &str, path: &str) -> Option<String> {
    let out = git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{commit}:{path}"),
        ],
    )
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn bytes_at(dir: &Path, commit: &str, path: &str) -> Option<Vec<u8>> {
    let out = git(dir, &["show", &format!("{commit}:{path}")]).ok()?;
    out.status.success().then_some(out.stdout)
}

/// Whether `path` differs between two commits (present/absent included).
pub(crate) fn changed(dir: &Path, from: &str, to: &str, path: &str) -> bool {
    blob(dir, from, path) != blob(dir, to, path)
}

/// Commits in `from..to` touching `path`, oldest first.
pub(crate) fn commits_touching(
    dir: &Path,
    from: &str,
    to: &str,
    path: &str,
) -> Result<Vec<String>, MusterError> {
    git_lines(
        dir,
        &[
            "rev-list",
            "--reverse",
            &format!("{from}..{to}"),
            "--",
            path,
        ],
    )
}

/// The signed state at `commit`, when it has a parseable one.
pub(crate) fn state_at(dir: &Path, commit: &str) -> Option<fleet_state::FleetState> {
    let bytes = bytes_at(dir, commit, STATE_FILE)?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn manifest_at(dir: &Path, commit: &str) -> Option<Manifest> {
    let bytes = bytes_at(dir, commit, manifest::FILE_NAME)?;
    Manifest::parse(&String::from_utf8(bytes).ok()?, manifest::FILE_NAME).ok()
}

/// What a replay may rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The abandoned commits are this machine's own never-accepted seals
    /// (the caller's rollback rule held): files, policies and secrets are
    /// replayed and the result re-sealed with this machine's key.
    Signed,
    /// Nothing proves who wrote the abandoned commits: only files outside
    /// the signed state are replayed, nothing is ever sealed, and every
    /// signed or governance-coupled change is reported instead.
    UnsignedOnly,
}

/// Replays `fork.base..fork.local` onto the working repository (at
/// `fork.accepted`) and commits it (re-sealed for [`Scope::Signed`]).
/// Returns what was replayed and what collided; a hard error leaves the
/// caller to restore the accepted state.
pub(crate) fn replay(ctx: &mut Context, fork: &Fork, scope: Scope) -> Result<Replay, MusterError> {
    let mut out = Replay::default();
    replay_files(ctx, fork, &mut out)?;
    let message = "fix(muster): replay unpublished local changes on the accepted fleet state";
    match scope {
        Scope::Signed => {
            replay_manifest(ctx, fork, scope, &mut out)?;
            replay_vault(ctx, fork, &mut out)?;
            replay_policies(ctx, fork, &mut out)?;
            injected_crash("after-replay");
            crate::trust::commit(ctx, message)?;
        }
        Scope::UnsignedOnly => {
            // Only the unsigned part of muster.toml (never its governance,
            // policies included); nothing in the vault.
            replay_manifest(ctx, fork, scope, &mut out)?;
            let dir = &ctx.config_dir;
            for path in changed_paths(dir, fork, &["vault", STATE_FILE])? {
                out.collide(
                    format!("file {path}"),
                    "it is part of the signed fleet state and was not re-applied; redo the \
                     command that changed it",
                );
            }
            injected_crash("after-replay");
            crate::git::commit_if_configured(dir, message)?;
        }
    }
    Ok(out)
}

/// Test hook (debug builds only; release builds compile it out, so the
/// variable is ignored there): ends the process at `point`, simulating a
/// crash in the middle of a reconcile.
pub(crate) fn injected_crash(point: &str) {
    #[cfg(debug_assertions)]
    if std::env::var("MUSTER_TEST_PULL_CRASH").is_ok_and(|v| v == point) {
        std::process::exit(86);
    }
    let _ = point;
}

/// Paths under `pathspecs` that differ between `fork.base` and `fork.local`.
fn changed_paths(dir: &Path, fork: &Fork, pathspecs: &[&str]) -> Result<Vec<String>, MusterError> {
    let mut args = vec![
        "diff",
        "--name-only",
        "--no-renames",
        fork.base.as_str(),
        fork.local.as_str(),
        "--",
    ];
    args.extend(pathspecs);
    git_lines(dir, &args)
}

// ---------------------------------------------------------------------------
// Non-signed files
// ---------------------------------------------------------------------------

/// Paths never replayed as files: the signed state itself, and enrollment
/// requests, whose approvals and removal are coupled to the governance in
/// `muster.toml` (an approval replayed without its governance change, or a
/// request deleted without the enrollment, would leave the two out of step).
const SIGNED_PATHS: [&str; 4] = [
    ":(exclude)fleet.state.json",
    ":(exclude)vault",
    ":(exclude)muster.toml",
    ":(exclude)enroll",
];

fn replay_files(ctx: &Context, fork: &Fork, out: &mut Replay) -> Result<(), MusterError> {
    let dir = ctx.config_dir.as_path();
    let mut args = vec![".".to_owned()];
    args.extend(SIGNED_PATHS.map(str::to_owned));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    for path in changed_paths(dir, fork, &args)? {
        replay_file(dir, fork, &path, out)?;
    }
    for path in changed_paths(dir, fork, &["enroll"])? {
        if own_fresh_request(ctx, fork, &path) {
            replay_file(dir, fork, &path, out)?;
            continue;
        }
        // Approvals are signed by the command itself: repeating it on top of
        // the accepted request adds this one again.
        let hint = match path
            .strip_prefix("enroll/")
            .and_then(|p| p.strip_suffix(".toml"))
        {
            Some(machine) => format!(
                "repeat `muster approve {machine}` on top of the accepted request (or `muster \
                 enroll --name {machine}` on the requesting machine)"
            ),
            None => "redo the enrollment command that changed it".to_owned(),
        };
        out.collide(format!("file {path}"), hint);
    }
    Ok(())
}

/// Whether `path` holds, both before (if at all) and after this machine's
/// change, its own enrollment request for its own keys and without any
/// approval: creating or refreshing it changes no governance, so it is
/// replayed like any file. Approvals, and the removal that enrolls, are not.
fn own_fresh_request(ctx: &Context, fork: &Fork, path: &str) -> bool {
    let dir = &ctx.config_dir;
    let (Ok(identity), Ok(Some(signer))) = (
        Identity::load(&ctx.state_dir),
        crate::signing::Signer::load(&ctx.state_dir),
    ) else {
        return false;
    };
    let own = |commit: &str| {
        request_at(dir, commit, path).is_some_and(|r| {
            crate::fleet::EnrollmentRequest::rel_path(&r.name) == path
                && r.recipient == identity.recipient()
                && r.signing_key.as_deref() == Some(signer.public().as_str())
                && r.approvals.is_empty()
        })
    };
    own(&fork.local) && (blob(dir, &fork.base, path).is_none() || own(&fork.base))
}

/// Three-way patch of one file's `base..local` change onto the working tree;
/// a collision keeps the accepted version.
fn replay_file(dir: &Path, fork: &Fork, path: &str, out: &mut Replay) -> Result<(), MusterError> {
    let patch = git(
        dir,
        &[
            "diff",
            "--binary",
            "--no-renames",
            &fork.base,
            &fork.local,
            "--",
            path,
        ],
    )?;
    if !patch.status.success() {
        return Err(op_err("git diff"));
    }
    let hint = "copy this machine's version back from the backup ref and commit it";
    let tracked = blob(dir, "HEAD", path).is_some();
    // A file the accepted state lacks is only ever created where nothing
    // exists yet (an untracked file of the user's is never overwritten or
    // removed), and only below real directories.
    if !tracked && !vacant(dir, path) {
        out.collide(format!("file {path}"), hint);
        return Ok(());
    }
    if apply_patch(dir, &patch.stdout)? {
        out.replayed.push(format!("file {path}"));
        return Ok(());
    }
    // Keep the accepted version of this one file; the abandoned version
    // stays in the backup ref. Only git touches the path (it refuses to
    // write beyond a symlink). A failed patch of a file the accepted state
    // lacks leaves nothing behind: its index entry and whatever it wrote at
    // the (vacant) path are dropped.
    if tracked {
        let _ = git(dir, &["checkout", "--quiet", "-f", "HEAD", "--", path]);
    } else {
        let _ = git(
            dir,
            &["rm", "--quiet", "--cached", "--ignore-unmatch", "--", path],
        );
        remove_leaf(dir, path);
    }
    out.collide(format!("file {path}"), hint);
    Ok(())
}

/// Whether `rel` names nothing yet and every existing parent below `dir` is
/// a real directory (no symlink, no file).
fn vacant(dir: &Path, rel: &str) -> bool {
    real_parents(dir, rel) && dir.join(rel).symlink_metadata().is_err()
}

fn real_parents(dir: &Path, rel: &str) -> bool {
    let rel = Path::new(rel);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return false;
    }
    let mut at = dir.to_path_buf();
    let mut parents: Vec<_> = rel.components().collect();
    parents.pop();
    for component in parents {
        at.push(component);
        match at.symlink_metadata() {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return false,
            Err(_) => return true,
        }
    }
    true
}

/// Removes the file (or link) a failed patch created at a path that was
/// vacant before it ran; never follows a symlink, never touches a directory.
pub(crate) fn remove_leaf(dir: &Path, rel: &str) {
    if !real_parents(dir, rel) {
        return;
    }
    let path = dir.join(rel);
    if path.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
        let _ = std::fs::remove_file(path);
    }
}

fn apply_patch(dir: &Path, patch: &[u8]) -> Result<bool, MusterError> {
    use std::io::Write;
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["apply", "--3way", "--whitespace=nowarn", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| op_err("cannot run git apply"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(patch)
            .map_err(|_| op_err("cannot feed git apply"))?;
    }
    let status = child.wait().map_err(|_| op_err("git apply"))?;
    Ok(status.success())
}

// ---------------------------------------------------------------------------
// muster.toml
// ---------------------------------------------------------------------------

/// Governance content without the per-secret policies (merged separately).
fn governance_without_policies(m: &Manifest) -> Value {
    let mut content = fleet_state::governance_content(m);
    if let Some(map) = content.as_object_mut() {
        map.remove("secrets");
    }
    content
}

/// The manifest with what `sync` and the secret commands rewrite on their
/// own (bookkeeping and policies) cleared, to tell user edits apart.
fn without_bookkeeping(m: &Manifest) -> Manifest {
    let mut m = m.clone();
    m.secrets.clear();
    for machine in m.machines.values_mut() {
        machine.last_sync = None;
        machine.applied_profile = None;
        machine.muster_version.clear();
        machine.agents.clear();
    }
    m
}

/// `m` with every governance field (what [`fleet_state::governance_content`]
/// signs, except the per-secret policies) taken from `from`: what is left
/// differing are the other edits.
fn with_governance_of(m: &Manifest, from: &Manifest) -> Manifest {
    let mut m = m.clone();
    m.revoked_recipients = from.revoked_recipients.clone();
    m.revoked_signing_keys = from.revoked_signing_keys.clone();
    m.recovery = from.recovery.clone();
    m.machines = from.machines.clone();
    m.members = from.members.clone();
    m.groups = from.groups.clone();
    m.ai_providers = from.ai_providers.clone();
    m.mcp_server = from.mcp_server.clone();
    m.team = from.team.clone();
    for (name, profile) in m.profiles.iter_mut() {
        profile.ai_providers = from
            .profiles
            .get(name)
            .map(|p| p.ai_providers.clone())
            .unwrap_or_default();
    }
    m
}

fn replay_manifest(
    ctx: &Context,
    fork: &Fork,
    scope: Scope,
    out: &mut Replay,
) -> Result<(), MusterError> {
    let dir = &ctx.config_dir;
    if !changed(dir, &fork.base, &fork.local, manifest::FILE_NAME) {
        return Ok(());
    }
    let (Some(base), Some(local)) = (manifest_at(dir, &fork.base), manifest_at(dir, &fork.local))
    else {
        out.collide(
            "muster.toml",
            "this machine's muster.toml could not be read; re-apply its edits by hand",
        );
        return Ok(());
    };
    let current = Manifest::load(dir)?;
    // What the text merge may never change: the governance, and without a
    // re-seal the per-secret policies too.
    let signed = |m: &Manifest| match scope {
        Scope::Signed => governance_without_policies(m),
        Scope::UnsignedOnly => fleet_state::governance_content(m),
    };
    if signed(&base) != signed(&local) {
        out.collide(
            "muster.toml governance (machines, members, groups or keys)",
            "re-run the governance command on top of the accepted state",
        );
    }
    let merged = merge_text(ctx, fork)?
        .and_then(|text| Manifest::parse(&text, manifest::FILE_NAME).ok())
        .filter(|m| signed(m) == signed(&current));
    let mut next = match merged {
        Some(m) => m,
        None => {
            // Reported even next to a governance collision: the edits made
            // together with it are dropped with it.
            if without_bookkeeping(&with_governance_of(&local, &base)) != without_bookkeeping(&base)
            {
                out.collide(
                    "muster.toml",
                    "this machine's other muster.toml edits could not be merged and were not \
                     kept; copy them back from the backup ref",
                );
            }
            current.clone()
        }
    };
    // Secret policies are replayed after the vault (`replay_policies`), and
    // never without a re-seal.
    next.secrets = current.secrets.clone();
    if next != current {
        next.save(dir)?;
    }
    Ok(())
}

/// Secret policies (`[secrets]`), one reference at a time, after the vault:
/// a policy follows its secret's fate. When this machine's change to the
/// secret collided, or the accepted history changed or removed the secret,
/// its policy is not replayed either (an allow list written for one value
/// never lands on another), and is reported.
fn replay_policies(ctx: &Context, fork: &Fork, out: &mut Replay) -> Result<(), MusterError> {
    let dir = &ctx.config_dir;
    if !changed(dir, &fork.base, &fork.local, manifest::FILE_NAME) {
        return Ok(());
    }
    let (Some(base), Some(local)) = (manifest_at(dir, &fork.base), manifest_at(dir, &fork.local))
    else {
        return Ok(());
    };
    let current = Manifest::load(dir)?;
    let identity = Identity::load(&ctx.state_dir)?;
    let collided: BTreeSet<String> = out.collisions.iter().map(|c| c.subject.clone()).collect();
    let replayed: BTreeSet<String> = out.replayed.iter().cloned().collect();
    let mut policies = current.secrets.clone();
    let references: BTreeSet<&String> = base.secrets.keys().chain(local.secrets.keys()).collect();
    for reference in references {
        let (before, after) = (base.secrets.get(reference), local.secrets.get(reference));
        if before == after {
            continue;
        }
        let now = current.secrets.get(reference);
        if now == after {
            continue;
        }
        let secret = reference
            .strip_prefix("native://")
            .and_then(|key| key.split_once('/'));
        let fate_blocks = secret.is_some_and(|(group, name)| {
            let key = format!("{group}/{name}");
            collided.contains(&key)
                || (!replayed.contains(&key) && accepted_changed(dir, fork, group, name, &identity))
        });
        if now != before || fate_blocks {
            out.collide(
                format!("policy {reference}"),
                format!(
                    "re-run `muster secret allow {reference}` once its secret is settled on \
                     the accepted state"
                ),
            );
            continue;
        }
        match after {
            Some(policy) => policies.insert(reference.clone(), policy.clone()),
            None => policies.remove(reference),
        };
        out.replayed.push(format!("policy {reference}"));
    }
    if policies != current.secrets {
        let mut next = current;
        next.secrets = policies;
        next.save(dir)?;
    }
    Ok(())
}

/// `git merge-file` of the accepted, base and local `muster.toml`; `None`
/// on a textual conflict.
fn merge_text(ctx: &Context, fork: &Fork) -> Result<Option<String>, MusterError> {
    let dir = &ctx.config_dir;
    let scratch = ctx.state_dir.join("cloud").join(format!(
        "replay-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    std::fs::create_dir_all(&scratch).map_err(|_| op_err("cannot prepare a merge"))?;
    let result = (|| {
        let ours = scratch.join("accepted");
        let base = scratch.join("base");
        let theirs = scratch.join("local");
        let current = std::fs::read(dir.join(manifest::FILE_NAME))
            .map_err(|_| op_err("cannot read muster.toml"))?;
        crate::vault::write_atomic(&ours, &current)?;
        crate::vault::write_atomic(
            &base,
            &bytes_at(dir, &fork.base, manifest::FILE_NAME).unwrap_or_default(),
        )?;
        crate::vault::write_atomic(
            &theirs,
            &bytes_at(dir, &fork.local, manifest::FILE_NAME).unwrap_or_default(),
        )?;
        let path = |p: &Path| p.to_str().map(str::to_owned).ok_or_else(|| op_err("path"));
        let (o, b, t) = (path(&ours)?, path(&base)?, path(&theirs)?);
        let merged = git(dir, &["merge-file", "-p", &o, &b, &t])?;
        Ok(match merged.status.code() {
            Some(0) => String::from_utf8(merged.stdout).ok(),
            _ => None,
        })
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

// ---------------------------------------------------------------------------
// The vault
// ---------------------------------------------------------------------------

fn vault_groups(dir: &Path, commit: &str) -> Result<BTreeSet<String>, MusterError> {
    let mut groups = BTreeSet::new();
    for path in git_lines(dir, &["ls-tree", "--name-only", commit, "vault/"])? {
        let name = path.strip_prefix("vault/").unwrap_or(&path);
        if let Some(group) = name
            .strip_suffix(".age")
            .or_else(|| name.strip_suffix(".meta.json"))
        {
            groups.insert(group.to_owned());
        }
    }
    Ok(groups)
}

type Values = BTreeMap<String, SecretValue>;

fn values_of(group: &str, armored: Option<Vec<u8>>, id: &Identity) -> Result<Values, MusterError> {
    match armored {
        None => Ok(BTreeMap::new()),
        Some(bytes) => crate::vault::decrypt_values(group, &bytes, id),
    }
}

/// The plaintext of `name`, borrowed (never copied out of the map).
fn exposed<'a>(values: &'a Values, name: &str) -> Option<&'a str> {
    values.get(name).map(SecretValue::expose)
}

fn replay_vault(ctx: &mut Context, fork: &Fork, out: &mut Replay) -> Result<(), MusterError> {
    let dir = ctx.config_dir.clone();
    let mut groups = vault_groups(&dir, &fork.base)?;
    groups.extend(vault_groups(&dir, &fork.local)?);
    let identity = Identity::load(&ctx.state_dir)?;
    for group in groups {
        let age = format!("vault/{group}.age");
        let meta = format!("vault/{group}.meta.json");
        if !changed(&dir, &fork.base, &fork.local, &age)
            && !changed(&dir, &fork.base, &fork.local, &meta)
        {
            continue;
        }
        let before = values_of(&group, bytes_at(&dir, &fork.base, &age), &identity);
        let after = values_of(&group, bytes_at(&dir, &fork.local, &age), &identity);
        let (Ok(before), Ok(after)) = (before, after) else {
            out.collide(
                format!("vault group {group}"),
                format!("this machine cannot read its own change to group {group}; redo it"),
            );
            continue;
        };
        // This machine's intent, one secret at a time.
        let mut intent: Vec<(String, Option<SecretValue>)> = Vec::new();
        for (name, value) in &after {
            if exposed(&before, name) != Some(value.expose()) {
                intent.push((name.clone(), Some(value.clone())));
            }
        }
        for name in before.keys().filter(|n| !after.contains_key(*n)) {
            intent.push((name.clone(), None));
        }
        if intent.is_empty() {
            continue;
        }
        let current_bytes = std::fs::read(dir.join(&age)).ok();
        let Ok(mut current) = values_of(&group, current_bytes, &identity) else {
            for (name, _) in &intent {
                out.collide(
                    format!("{group}/{name}"),
                    "this machine cannot read the accepted group; ask a reader to redo it",
                );
            }
            continue;
        };
        // Every version the accepted history went through since the fork:
        // a key it changed at any point (even back to its old value, like a
        // secret added then removed) is never replayed over it.
        let history = accepted_history(&dir, fork, &group, &identity);
        for (name, target) in intent {
            let key = format!("{group}/{name}");
            let wanted = target.as_ref().map(SecretValue::expose);
            let now = exposed(&current, &name);
            if now == wanted {
                continue;
            }
            let was = exposed(&before, &name);
            let touched = history
                .as_ref()
                .is_none_or(|versions| versions.iter().any(|v| exposed(v, &name) != was));
            if now != was || touched {
                let hint = match (&target, now) {
                    (Some(_), None) => {
                        format!("to keep this machine's value run `muster secret add {key}` again")
                    }
                    (Some(_), Some(_)) => format!(
                        "to keep this machine's value run `muster secret rotate {key}` again"
                    ),
                    (None, _) => format!("to remove it run `muster secret rm {key}` again"),
                };
                out.collide(key, hint);
                continue;
            }
            match apply_secret(ctx, &group, &name, target.clone(), &identity) {
                Ok(()) => {
                    match target {
                        Some(v) => current.insert(name, v),
                        None => current.remove(&name),
                    };
                    out.replayed.push(key);
                }
                Err(e) => out.collide(
                    key.clone(),
                    format!("it could not be re-applied ({e}); run the secret command again"),
                ),
            }
        }
    }
    Ok(())
}

/// The versions of `group` the SIGNED chain of the accepted history went
/// through since the fork, newest first, decrypted: from the verified tip's
/// state back along `prev` to the fork base's state, each epoch's ciphertext
/// is taken from a commit carrying that exact state and accepted only when
/// its sha256 is the vault digest that state signs. Commit metadata, tree
/// layout and history simplification therefore play no part. `None` when an
/// epoch cannot be authenticated or read: every key then counts as changed.
fn accepted_history(dir: &Path, fork: &Fork, group: &str, id: &Identity) -> Option<Vec<Values>> {
    let age = format!("vault/{group}.age");
    let base = state_at(dir, &fork.base)?;
    let base_hash = base.payload_hash().ok()?;
    // The fork base's own version must be the one its state signs, too.
    authentic(dir, &fork.base, &age, base.payload.vault.get(group))?;
    let commits = git_lines(
        dir,
        &["rev-list", &format!("{}..{}", fork.base, fork.accepted)],
    )
    .ok()?;
    let mut by_state: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for commit in commits {
        if let Some(hash) = state_at(dir, &commit).and_then(|s| s.payload_hash().ok()) {
            by_state.entry(hash).or_default().push(commit);
        }
    }
    let mut versions = Vec::new();
    let mut state = state_at(dir, &fork.accepted)?;
    for _ in 0..=by_state.len() {
        let hash = state.payload_hash().ok()?;
        if hash == base_hash {
            return Some(versions);
        }
        let digest = state.payload.vault.get(group);
        let bytes = by_state
            .get(&hash)?
            .iter()
            .find_map(|commit| authentic(dir, commit, &age, digest))?;
        versions.push(values_of(group, bytes, id).ok()?);
        let prev = state.payload.prev.clone()?;
        if prev == base_hash {
            return Some(versions);
        }
        let carrier = by_state.get(&prev)?.first()?;
        state = state_at(dir, carrier)?;
    }
    None
}

/// The bytes of `path` at `commit` when they match the signed `digest`
/// (`Some(None)`: absent where the state lists no such group).
fn authentic(
    dir: &Path,
    commit: &str,
    path: &str,
    digest: Option<&String>,
) -> Option<Option<Vec<u8>>> {
    let bytes = bytes_at(dir, commit, path);
    match (digest, bytes) {
        (None, None) => Some(None),
        (Some(digest), Some(bytes)) if crate::hash::sha256_hex(&bytes) == *digest => {
            Some(Some(bytes))
        }
        _ => None,
    }
}

/// Whether the accepted history changed `group/name` since the fork, at any
/// signed epoch (or cannot be shown not to have).
fn accepted_changed(dir: &Path, fork: &Fork, group: &str, name: &str, id: &Identity) -> bool {
    let age = format!("vault/{group}.age");
    let Ok(before) = values_of(group, bytes_at(dir, &fork.base, &age), id) else {
        return true;
    };
    let was = exposed(&before, name);
    accepted_history(dir, fork, group, id)
        .is_none_or(|versions| versions.iter().any(|v| exposed(v, name) != was))
}

fn apply_secret(
    ctx: &Context,
    group: &str,
    name: &str,
    value: Option<SecretValue>,
    identity: &Identity,
) -> Result<(), MusterError> {
    let mut vault =
        Vault::open(&ctx.config_dir)?.with_actor(crate::machine::load_name(&ctx.state_dir));
    let exists = vault.groups()?.iter().any(|g| g == group)
        && vault.list(group)?.iter().any(|s| s.name == name);
    match value {
        Some(value) if exists => vault.rotate(group, name, value, identity),
        Some(value) => {
            let recipients =
                crate::commands::secret::resolve_recipients(ctx, &vault, group, identity)?;
            vault.add(group, name, value, &recipients, identity)
        }
        None if exists => vault.remove(group, name, identity),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Superseded own enrollment request
// ---------------------------------------------------------------------------

/// The object id of `path` at `commit` when it is exactly one regular file
/// (mode 100644/100755). A symlink, a directory, a submodule or a path
/// reached through a symlinked parent never qualifies: git trees do not
/// follow symlinks, so nothing outside the named path is ever read.
pub(crate) fn regular_blob(dir: &Path, commit: &str, path: &str) -> Option<String> {
    let out = git(dir, &["ls-tree", "-z", commit, "--", path]).ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut entries = text.split('\0').filter(|e| !e.is_empty());
    let entry = entries.next()?;
    if entries.next().is_some() {
        return None;
    }
    let (meta, name) = entry.split_once('\t')?;
    let mut fields = meta.split(' ');
    let (mode, kind, oid) = (fields.next()?, fields.next()?, fields.next()?);
    (name == path && kind == "blob" && matches!(mode, "100644" | "100755")).then(|| oid.to_owned())
}

/// The enrollment request at `path` of `commit`, when it is a regular file
/// that parses.
pub(crate) fn request_at(
    dir: &Path,
    commit: &str,
    path: &str,
) -> Option<crate::fleet::EnrollmentRequest> {
    let oid = regular_blob(dir, commit, path)?;
    let out = git(dir, &["cat-file", "blob", &oid]).ok()?;
    if !out.status.success() {
        return None;
    }
    toml::from_str(&String::from_utf8(out.stdout).ok()?).ok()
}

/// Rewrites the linear history `base..local` so that no commit changes
/// `path` (it keeps its `base` version, or stays absent). Every commit is
/// kept with its author, committer, dates and message, even one left empty,
/// so the local history still records what this machine did. Only new
/// objects are written: no ref, index or working file is touched. `None`
/// when the range has merges or `path` is not a regular file at `base`.
pub(crate) fn without_path(
    dir: &Path,
    scratch: &Path,
    base: &str,
    local: &str,
    path: &str,
) -> Result<Option<String>, MusterError> {
    if !git_lines(dir, &["rev-list", "--merges", &format!("{base}..{local}")])?.is_empty() {
        return Ok(None);
    }
    let at_base = match git(dir, &["ls-tree", "-z", base, "--", path])?
        .stdout
        .is_empty()
    {
        true => None,
        false => match regular_blob(dir, base, path) {
            Some(oid) => Some(oid),
            None => return Ok(None),
        },
    };
    std::fs::create_dir_all(scratch).map_err(|_| op_err("cannot prepare a rewrite"))?;
    let index = scratch.join(format!(
        "index-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let result = (|| {
        let indexed = |args: &[&str]| -> Result<std::process::Output, MusterError> {
            let out = Command::new("git")
                .current_dir(dir)
                .env("GIT_INDEX_FILE", &index)
                .args(args)
                .output()
                .map_err(|_| op_err("cannot run git"))?;
            if !out.status.success() {
                return Err(op_err(&format!("git {}", args.first().unwrap_or(&""))));
            }
            Ok(out)
        };
        let mut parent = base.to_owned();
        for commit in git_lines(dir, &["rev-list", "--reverse", &format!("{base}..{local}")])? {
            indexed(&["read-tree", &commit])?;
            match &at_base {
                Some(oid) => {
                    indexed(&[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("100644,{oid},{path}"),
                    ])?;
                }
                None => {
                    indexed(&["update-index", "--force-remove", "--", path])?;
                }
            }
            let tree = String::from_utf8_lossy(&indexed(&["write-tree"])?.stdout)
                .trim()
                .to_owned();
            let meta = git(
                dir,
                &[
                    "log",
                    "-1",
                    "--date=raw",
                    "--format=%an%x00%ae%x00%ad%x00%cn%x00%ce%x00%cd%x00%B",
                    &commit,
                ],
            )?;
            let meta = String::from_utf8_lossy(&meta.stdout).into_owned();
            let f: Vec<&str> = meta.splitn(7, '\0').collect();
            if f.len() != 7 {
                return Err(op_err("cannot read a local commit"));
            }
            use std::io::Write;
            let mut child = Command::new("git")
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", f[0])
                .env("GIT_AUTHOR_EMAIL", f[1])
                .env("GIT_AUTHOR_DATE", f[2])
                .env("GIT_COMMITTER_NAME", f[3])
                .env("GIT_COMMITTER_EMAIL", f[4])
                .env("GIT_COMMITTER_DATE", f[5])
                .args(["commit-tree", &tree, "-p", &parent, "-F", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| op_err("cannot run git commit-tree"))?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(f[6].as_bytes())
                    .map_err(|_| op_err("git commit-tree"))?;
            }
            let out = child
                .wait_with_output()
                .map_err(|_| op_err("git commit-tree"))?;
            if !out.status.success() {
                return Err(op_err("git commit-tree"));
            }
            parent = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        }
        Ok(Some(parent))
    })();
    let _ = std::fs::remove_file(&index);
    result
}
