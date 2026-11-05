# ADR 0005: Cryptographic Fleet State Signing, Local Trust Anchors, and Signed Quorum

## Context

Muster synchronizes AI agent configurations and encrypted secrets across fleets using Git as the transport. Prior to this design, Git history served as the sole record of fleet governance. However, Git commit history does not provide cryptographic authorization guarantees:
1. **Rollback Attacks:** An attacker with write access to the central repository could force-push or commit an older, revoked configuration tree (rollback attack), reviving revoked age keys or stale security policies.
2. **Ciphertext Swapping & Forgery:** An adversary could replace encrypted `.age` files with crafted ciphertexts encrypted to unauthorized recipients without altering the Git commit tree in a verifiable manner.
3. **Unauthenticated Governance Edits:** Hand-edits or forged commits altering team quorum, member roles, or reader access lists could be merged without a digital signature from an authorized admin.
4. **Forged Quorum Approvals:** Enrollment requests previously recorded approvals as bare strings (`approvals = ["alice"]`), allowing any committer to fabricate quorum approvals.

## Decision

We introduce end-to-end cryptographic state signing, external local trust pinning, anti-rollback verification, and signed cryptographic quorum.

### 1. Per-Machine and Recovery Ed25519 Signing Keys
- Each enrolled machine generates a dedicated Ed25519 private signing key (`<state_dir>/signing.key`, mode 0600 in a 0700 directory).
- The public key is formatted as `musterpk1<64-hex>` and published in `muster.toml` under `[machines.<name>] signing_key`.
- The break-glass age recovery identity deterministically derives an Ed25519 signing key using HKDF-SHA256 with info salt `b"muster recovery signing v1"`, allowing emergency administrative operations from an unapproved machine.

### 2. Signed Fleet State (`fleet.state.json`)
At the root of the configuration repository, `fleet.state.json` contains:
- `version`: Protocol version (1).
- `epoch`: Strictly monotonic unsigned 64-bit counter (`u64`).
- `prev`: SHA-256 digest of the previous state payload (`null` at epoch 1).
- `governance`: SHA-256 digest of canonical JSON serialization of security-critical fields (`machines`, `members`, `groups`, `secrets`, `required_approvals`, `revoked_recipients`, `recovery`).
- `vault`: Map of group names to SHA-256 hashes of `<group>.age` ciphertext files.
- `signer`: Public key of the signing machine or recovery key.
- Detached Ed25519 signature over canonical JSON of the payload prefixed with domain separator `b"muster fleet state v1\n"`.

### 3. Canonical JSON Encoding
State hashing and signatures require byte-for-byte invariant serialization across platforms. We enforce canonical JSON:
- Keys sorted lexicographically at every object level.
- No whitespace ambiguity (compact format: `:` and `,`).
- Floats rejected to prevent IEEE 754 precision discrepancies.
- Validated via dedicated round-trip invariant tests.

### 4. External Local Trust Anchor (`<state_dir>/trust.json`)
Stored with permissions 0600 strictly outside the Git repository:
- Pins `last_accepted_epoch`, `payload_hash`, `governance` digest, and authorized admin/writer signing keys.
- **Anti-Rollback Verification:** Rejects states where `epoch < pin.epoch`, or `epoch == pin.epoch` with a mismatched payload hash (fork).
- **Anti-Downgrade Verification:** Refuses operation if `fleet.state.json` is missing when a local trust pin exists.
- **Automatic Rollback Restoration on Pull:** When `muster sync --pull` encounters an invalid state or rollback, it immediately reverts HEAD to the pre-pull commit so tainted or rolled-back state is never applied to the local machine.
- Advances pin atomically only after full signature, governance, and vault digest checks pass.

### 5. Automatic Re-Sealing vs. Operator Re-Signing (`muster trust resign`)
- **Automated Operations:** Every mutating security command (`init`, `approve`, `reject`, `revoke`, `secret add/rotate/rm`, `group rekey`, etc.) automatically calculates canonical digests, re-seals with `epoch + 1`, updates the local trust pin, and commits `fleet.state.json` in the same atomic Git commit.
- **Manual Governance Edits:** If an operator edits `muster.toml` by hand, commands consuming security state fail with code 5 (`error[security]`). The operator inspects the proposed state transition and runs `muster trust resign --yes`, which cryptographically seals the updated governance under an authorized admin or recovery key.
- **Trust Inspection:** `muster trust show` renders current epoch, payload hash, signer fingerprints, governance status, and local pin health.

### 6. Signed Quorum Approvals
`EnrollmentRequest.approvals` records a list of `SignedApproval` structs:
- Signature calculated over canonical JSON of `(name, recipient, signing_key, member, role, profile, created_at, nonce, "muster enroll approval v1")`.
- Random per-request `nonce` prevents replay across re-enrollments.
- Tampering with any bound field (e.g. swapping recipient) immediately invalidates previous signatures.
- Quorum counts only distinct active admin members whose signature verifies against the machine's published signing key in verified governance.
- Legacy string approvals are ignored with a warning diagnostic (`legacy_approval_ignored`) and never count toward quorum.

## Status

Accepted and implemented in `hillclimb/secrets-hardening`.
