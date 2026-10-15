//! Runs the `install.sh` shell test suite as part of `cargo test`.
//!
//! The suite itself is `tests/test_install_script.sh`; it mocks `curl`, `wget`
//! and `uname` so it needs no network access.

#[cfg(unix)]
#[test]
fn install_script_suite_passes() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/test_install_script.sh");
    let status = std::process::Command::new("bash")
        .arg(script)
        .status()
        .expect("failed to spawn bash for the install.sh test suite");
    assert!(status.success(), "install.sh test suite failed");
}
