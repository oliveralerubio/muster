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

fn forge(dir: &std::path::Path, file: &str, payload: &serde_json::Value, to: &Identity) {
    use std::io::Write;
    let recipient: age::x25519::Recipient = to.recipient().parse().unwrap();
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .unwrap();
    let mut out = Vec::new();
    let armor =
        age::armor::ArmoredWriter::wrap_output(&mut out, age::armor::Format::AsciiArmor).unwrap();
    let mut w = encryptor.wrap_output(armor).unwrap();
    w.write_all(payload.to_string().as_bytes()).unwrap();
    w.finish().unwrap().finish().unwrap();
    fs::write(dir.join("vault").join(file), out).unwrap();
}

fn is_security<T: std::fmt::Debug>(r: Result<T, muster::errors::MusterError>) -> bool {
    matches!(r, Err(muster::errors::MusterError::Security(_)))
}

#[test]
fn copying_one_group_over_another_is_rejected() {
    let d = tempdir().unwrap();
    let id = Identity::generate();
    let mut v = Vault::open(d.path()).unwrap();
    let rec = [id.recipient()];
    v.add("low", "n", SecretValue::new("low-value".into()), &rec, &id)
        .unwrap();
    v.add(
        "high",
        "n",
        SecretValue::new("high-value".into()),
        &rec,
        &id,
    )
    .unwrap();
    fs::copy(
        d.path().join("vault/low.age"),
        d.path().join("vault/high.age"),
    )
    .unwrap();
    assert!(is_security(v.get("high", "n", &id)));
    assert!(is_security(v.rotate(
        "high",
        "n",
        SecretValue::new("x".into()),
        &id
    )));
    assert!(is_security(v.add(
        "high",
        "m",
        SecretValue::new("x".into()),
        &rec,
        &id
    )));
}

#[test]
fn forged_payload_group_never_writes_outside_the_vault() {
    let d = tempdir().unwrap();
    let outer = d.path().join("cfg");
    fs::create_dir_all(&outer).unwrap();
    let id = Identity::generate();
    let mut v = Vault::open(&outer).unwrap();
    let rec = [id.recipient()];
    v.add("g", "n", SecretValue::new("x".into()), &rec, &id)
        .unwrap();
    let payload = serde_json::json!({
        "group": "../../x",
        "recipients": rec,
        "secrets": {"n": {"name": "n", "ciphertext_or_value": "y",
            "created_at": "t", "updated_at": "t"}}
    });
    forge(&outer, "g.age", &payload, &id);
    assert!(is_security(v.rotate(
        "g",
        "n",
        SecretValue::new("z".into()),
        &id
    )));
    assert!(!d.path().join("x.age").exists());
    assert!(!d.path().join("x.meta.json").exists());
}

#[test]
fn forged_secret_key_not_matching_entry_name_is_rejected() {
    let d = tempdir().unwrap();
    let id = Identity::generate();
    let mut v = Vault::open(d.path()).unwrap();
    let rec = [id.recipient()];
    v.add("g", "n", SecretValue::new("x".into()), &rec, &id)
        .unwrap();
    let payload = serde_json::json!({
        "group": "g",
        "recipients": rec,
        "secrets": {"../evil": {"name": "../evil", "ciphertext_or_value": "y",
            "created_at": "t", "updated_at": "t"}}
    });
    forge(d.path(), "g.age", &payload, &id);
    assert!(is_security(v.get("g", "n", &id)));
}
