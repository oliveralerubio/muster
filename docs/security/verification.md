# Security verification

Layered checks for the hardening done in commit 302c590. Each layer catches a
different class of bug.

| Layer | Tool | Where | What it covers |
| --- | --- | --- | --- |
| Property tests | `proptest` | `tests/test_security_properties.rs` | `is_valid_name`, `validate_bws_project`, `client_agent`, `approval_identity`, `write_atomic` (manifest + vault), recorded approvers; engine symlink checks live in `src/engine/mod.rs` tests |
| Fuzzing | `cargo-fuzz` (libFuzzer) | `fuzz/` | skill names, bws project IDs, `Manifest` and `SkillsLock` TOML parsing |
| Bounded model checking | Kani | `src/verification.rs` | no `..`, `/`, `\`, NUL in accepted names; no leading `-`/whitespace/control in bws IDs (inputs up to 6 bytes) |
| Signed fleet state | integration tests | `tests/test_fleet_state.rs`, `src/fleet_state.rs` tests | canonical JSON stability, signature and epoch chain, anti-rollback, deleted state, forged ciphertext, TOFU pin and `--expect-signer`, legacy migration, hand-edit refusal, signed approvals ([ADR 0005](../adr/0005-signed-fleet-state.md)) |
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

## Signed fleet state (ADR 0005)

```sh
cargo test --test test_fleet_state
muster trust show      # epoch, signer fingerprints, local pin
muster trust resign    # after an intentional hand edit of muster.toml
```

- Check the signer fingerprint out of band when a new machine first pins the
  fleet (or pass `muster sync --expect-signer SHA256:...`); the first pin is
  trust-on-first-use.
- Commands that consume security state (sync, status, doctor, secret
  get/add/rotate/rm, approve, revoke, rekey, member and group changes) verify
  `fleet.state.json` against `<state_dir>/trust.json` first. A rolled-back,
  forked, unsigned or tampered repo is refused, and a refused pull is undone.
- Enrollment approvals are Ed25519 signatures over the request (name,
  recipient, signing key, member, role, profile, created_at, nonce). Altering
  any of them invalidates earlier approvals; bare-name approvals never count.
- Not covered: no TLA+ model of the epoch chain yet (the existing model still
  abstracts crypto); `clientInfo` of an MCP client stays advisory, so pin the
  agent with `muster mcp-server --agent <name>` in MCP client configs (muster
  does not write its own MCP entry, so there is nothing to pin automatically);
  history is not walked when the remote jumps several epochs, any state signed
  by a key authorized under the last accepted state is accepted.
