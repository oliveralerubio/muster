use muster::{identity::Identity, secrets::SecretValue, vault::Vault};
use std::fs;
fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            files(&p, out);
        } else {
            out.push(p);
        }
    }
}
use tempfile::tempdir;

#[test]
fn identity_file_permissions_and_private_key_location() {
    let state = tempdir().unwrap();
    let config = tempdir().unwrap();
    let id = Identity::generate();
    id.save(state.path()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(state.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(state.path().join("identity.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(!config.path().join("identity.txt").exists());
    let loaded = Identity::load(state.path()).unwrap();
    assert_eq!(loaded.recipient(), id.recipient());
}
#[test]
fn add_rotate_remove_secret() {
    let d = tempdir().unwrap();
    let id = Identity::generate();
    let mut v = Vault::open(d.path()).unwrap();
    let rec = [id.recipient()];
    v.add("g", "n", SecretValue::new("first".into()), &rec, &id)
        .unwrap();
    assert_eq!(v.get("g", "n", &id).unwrap().expose(), "first");
    v.rotate("g", "n", SecretValue::new("second".into()), &id)
        .unwrap();
    assert_eq!(v.get("g", "n", &id).unwrap().expose(), "second");
    v.remove("g", "n", &id).unwrap();
    assert!(v.get("g", "n", &id).is_err());
}
#[test]
fn non_recipient_returns_typed_error() {
    let d = tempdir().unwrap();
    let id = Identity::generate();
    let other = Identity::generate();
    let mut v = Vault::open(d.path()).unwrap();
    v.add(
        "g",
        "n",
        SecretValue::new("x".into()),
        &[id.recipient()],
        &id,
    )
    .unwrap();
    assert!(v.get("g", "n", &other).is_err());
}
#[test]
fn reencrypt_replaces_reader_set() {
    let d = tempdir().unwrap();
    let a = Identity::generate();
    let b = Identity::generate();
    let mut v = Vault::open(d.path()).unwrap();
    v.add("g", "n", SecretValue::new("x".into()), &[a.recipient()], &a)
        .unwrap();
    v.reencrypt("g", &[b.recipient()], &a).unwrap();
    assert!(v.get("g", "n", &b).is_ok());
    assert!(v.get("g", "n", &a).is_err());
}
#[test]
fn resolve_native_reference_offline_and_reject_invalid() {
    let d = tempdir().unwrap();
    let id = Identity::generate();
    let mut v = Vault::open(d.path()).unwrap();
    v.add(
        "g",
        "n",
        SecretValue::new("x".into()),
        &[id.recipient()],
        &id,
    )
    .unwrap();
    assert_eq!(v.resolve("native://g/n", &id).unwrap().expose(), "x");
    assert!(v.resolve("invalid", &id).is_err());
}
#[test]
fn secret_never_leaks_to_format_metadata_or_files() {
    let d = tempdir().unwrap();
    let id = Identity::generate();
    let mut v = Vault::open(d.path()).unwrap();
    let value = "SENSITIVE_SEED_VALUE";
    let s = SecretValue::new(value.into());
    assert!(!format!("{s:?}{s}").contains(value));
    v.add("g", "n", s, &[id.recipient()], &id).unwrap();
    let json = serde_json::to_string(&v.list("g").unwrap()).unwrap();
    assert!(!json.contains(value));
    let mut paths = vec![];
    files(d.path(), &mut paths);
    for p in paths {
        assert!(!String::from_utf8_lossy(&fs::read(p).unwrap()).contains(value));
    }
}
