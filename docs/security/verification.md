# Security verification

Layered checks for the hardening done in commit 302c590. Each layer catches a
different class of bug.

| Layer | Tool | Where | What it covers |
| --- | --- | --- | --- |
| Property tests | `proptest` | `tests/test_security_properties.rs` | `is_valid_name`, `validate_bws_project`, `client_agent`, `approval_identity`, `write_atomic` (manifest + vault), recorded approvers; engine symlink checks live in `src/engine/mod.rs` tests |
| Fuzzing | `cargo-fuzz` (libFuzzer) | `fuzz/` | skill names, bws project IDs, `Manifest` and `SkillsLock` TOML parsing |
| Bounded model checking | Kani | `src/verification.rs` | no `..`, `/`, `\`, NUL in accepted names; no leading `-`/whitespace/control in bws IDs (inputs up to 6 bytes) |
| Protocol model | TLA+ / TLC | `formal/` | revoked machines never read the vault nor approve; quorum counts active admins only |

## Run

```sh
# Property tests (default 512 cases each)
cargo test --test test_security_properties

# Fuzzing (nightly + `cargo install cargo-fuzz`)
cargo +nightly fuzz list
cargo +nightly fuzz run fuzz_skill_name
scripts/fuzz_smoke.sh 200      # quick compile-and-run check of every target

# Kani (`cargo install --locked kani-verifier && cargo kani setup`)
cargo kani

# TLA+ (tla2tools.jar)
cd formal && tlc team_revocation.tla -config team_revocation.cfg
```

## Notes

- `validate_bws_project` rejects a leading `-`, whitespace, control characters
  and IDs longer than 64 bytes.
- `client_agent` is exact-match but case-insensitive (`Claude` == `claude`);
  prefixes, suffixes and substrings never match.
- `approval_identity` lowercases the SHA-256, so hex case does not change the
  approval; agent, version, URL and digest each do.
- The TLA+ model abstracts crypto: re-encryption is the atomic update of
  `vaultReaders`. It checks the protocol, not the age implementation.
