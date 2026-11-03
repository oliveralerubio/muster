//! Kani bounded model-checking proofs. Compiled only under `cargo kani`.
//! See `docs/security/verification.md`.

use crate::providers::cli::validate_bws_project;
use crate::skills::lock::is_valid_name;

const N: usize = 6;

fn bounded_str(bytes: &[u8; N], len: usize) -> Option<&str> {
    std::str::from_utf8(&bytes[..len]).ok()
}

/// Any accepted skill name is a single safe path component.
#[kani::proof]
#[kani::unwind(10)]
fn is_valid_name_blocks_path_traversal() {
    let bytes: [u8; N] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= N);
    if let Some(s) = bounded_str(&bytes, len) {
        if is_valid_name(s) {
            assert!(!s.contains(".."));
            assert!(!s.contains('/'));
            assert!(!s.contains('\\'));
            assert!(!s.contains('\0'));
        }
    }
}

/// Any accepted Bitwarden project ID cannot be parsed as a flag or smuggle
/// whitespace/control characters.
#[kani::proof]
#[kani::unwind(10)]
fn validate_bws_project_blocks_flag_injection() {
    let bytes: [u8; N] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= N);
    if let Some(s) = bounded_str(&bytes, len) {
        if validate_bws_project(s).is_ok() {
            assert!(!s.starts_with('-'));
            assert!(!s.chars().any(|c| c.is_whitespace() || c.is_control()));
        }
    }
}
