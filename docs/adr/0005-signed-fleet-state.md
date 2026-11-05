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
- `vault_meta`: Map of group names to SHA-256 hashes of `<group>.meta.json` sidecar files (plaintext metadata including recipient and secret lists). Tampered, missing, forged, or unexpected sidecars fail verification with security error code 5.
- `signer`: Public key of the signing machine or recovery key.
- Compatibility: `vault_meta` is omitted from the canonical bytes when empty, so states signed before sidecar coverage keep their exact hash and signature. Sidecars of such a state are not compared until the next change re-signs it with digests.
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
- **Pin check on every mutating command:** before committing, any mutating command (security-consuming or not, via `trust::guard` and `trust::commit`) refuses a repo whose `epoch < pin.epoch` ("cannot commit on top of a rolled-back fleet state"), so no command builds on a rolled-back checkout and pushes it.
- **TOFU confirmation record:** `trust.json` records `tofu: true` when the pin was created by trust on first use without `--expect-signer`. `muster doctor` reports it as the info diagnostic `trust_pin_unconfirmed` with the signer fingerprint to verify out of band. A later run with a matching `--expect-signer` clears the flag; pins from before this field and pins a machine created by signing itself count as confirmed.
- Advances pin atomically only after full signature, governance, and vault digest checks pass.

### 5. Automatic Re-Sealing vs. Operator Re-Signing (`muster trust resign`)
- **Automated Operations:** Every mutating security command (`init`, `approve`, `reject`, `revoke`, `secret add/rotate/rm`, `group rekey`, etc.) automatically calculates canonical digests, re-seals with `epoch + 1`, updates the local trust pin, and commits `fleet.state.json` in the same atomic Git commit.
- **Manual Governance Edits:** If an operator edits `muster.toml` by hand, commands consuming security state fail with code 5 (`error[security]`). The operator inspects the proposed state transition and runs `muster trust resign --yes`, which cryptographically seals the updated governance under an authorized admin or recovery key.
- **Resign anti-rollback invariant:** a seal (including `muster trust resign`, migration and automatic re-sealing) must advance or preserve the local pin epoch, never launder an older state. If the repo's `fleet.state.json` epoch is below the pinned epoch, resign is refused with a security error (code 5: "cannot re-sign a rolled-back fleet state (epoch N < pinned M)"); restore the repo from a trusted copy instead. The check runs inside `fleet_state::seal` and at the top of `trust resign`, before the "already up to date" shortcut.
- **Trust Inspection:** `muster trust show` renders current epoch, payload hash, signer fingerprints, governance status, and local pin health.

### 6. Signed Quorum Approvals
`EnrollmentRequest.approvals` records a list of `SignedApproval` structs:
- Signature calculated over canonical JSON of `(name, recipient, signing_key, member, role, profile, created_at, nonce, "muster enroll approval v1")`.
- Random per-request `nonce` prevents replay across re-enrollments.
- Tampering with any bound field (e.g. swapping recipient) immediately invalidates previous signatures.
- Quorum counts only distinct active admin members whose signature verifies against the machine's published signing key in verified governance.
- Legacy string approvals are ignored with a warning diagnostic (`legacy_approval_ignored`) and never count toward quorum.

### 7. Hardening (round 5)
- **Governance scope:** provider destinations (`kind`, `base_url`, `key`, `api_key_helper`, including profile overrides), `mcp_server.reveal` and an explicit `allow = []` (distinct from an absent list) are signed. New or changed provider destinations need approval at apply time (`ai_provider`); unattended apply exits 2.
- **Contiguous chain:** an epoch jump larger than one is verified by walking the git history of `fleet.state.json`: each intermediate state must be signed by a key its predecessor authorized and link by `prev`. Missing history is refused ("epoch jump without valid intermediate chain").
- **Bootstrap:** `--expect-signer` against an unsigned repo is a security error; it accepts only the full `musterpk1...` key or the full 256-bit `SHA256:` fingerprint.
- **Per-group writers:** a vault-only transition signed by a non-admin is accepted only if the signer's machine is a recipient of every group whose ciphertext or sidecar changed. The pin stores the vault baseline and a governance snapshot for this.
- **Signed quorum:** with `required_approvals > 1`, an added machine carries its signed admin approvals in the state (`enrollments`), and an activated member carries signed approvals in `Member.approvals` (`<admin>:<machine>:<signature>`). Bare names never count; the recovery key stands alone (break-glass). Group reader grants and role promotions are gated by the k-of-n co-signatures of section 8.
- **Recoverable revocation:** `revoke` and `member rm` stage all changes in a shadow directory under the state dir, validate the sealed result, write a journal, then copy files into place; the next command completes an interrupted journal (or discards an unjournaled stage) automatically.

### 8. Hardening (round 7)
- **k-of-n for every governance change:** when the prior accepted state has team mode and `required_approvals = k > 1`, any transition whose governance digest differs (additions, removals, role changes, policy, readers, providers, revoked list) needs `k` distinct admins of the *prior* state. `FleetState.cosignatures` (`{signer, signature}`, outside the signed payload, so the payload hash is unchanged) holds the extra signatures, each over `"muster fleet cosign v1\n" + payload bytes`. Votes count per admin member (two machines of one admin are one vote). Forged, non-admin and duplicate co-signatures do not count; the recovery key alone satisfies the quorum (break-glass); `k = 1` is unchanged. Vault-only changes keep the writer-reads-groups rule. Exemptions: a diff that only adds machines/members already carrying their own k signed approvals (the section 7 evidence), and dropping a never-active invitation.
- **Pending, fail closed:** a state short of `k` is *pending*. Peers neither fail hard nor advance the pin: they keep the last accepted state, emit the warning `governance_pending` (received/required plus the governance diff), and refuse every command except `sync` and `doctor` (`sync` still pulls to receive co-signatures and applies nothing). The proposer's own pin does not advance either, and nobody may seal on top of a state the machine has not accepted. `trust show` reports `pending_signatures`.
- **`muster trust cosign`:** an admin of the prior state (not the proposer) reviews the diff against its pin, confirms (TTY or `--yes`), appends its co-signature, commits and pushes; once `k` is reached the cosigner's pin advances and peers accept on their next pull. Cosign sequentially (pull first) to avoid merge conflicts on `fleet.state.json`. The history walk uses the latest version of each epoch, so intermediate epochs must have gathered their signatures.
- **First use everywhere:** the typed/`--expect-signer` confirmation runs inside `trust::verify`, so any command that would create a first pin from a non-self signer needs it. `--expect-signer` is a global flag and `MUSTER_EXPECT_SIGNER` an environment variable. `trust show` never pins.
- **Legacy migration:** automatic only for a repo this machine created alone: one machine in the manifest, no git remote, and no committed `muster.toml` that ever listed another machine. Pruning the manifest to one machine no longer skips the confirmation.
- **Journal binding:** before applying a revocation journal, the staged `fleet.state.json` must verify, the journal pin must equal the pin that state implies, and its signer/predecessor must be authorized by the current pin (epoch +1). Otherwise the transaction is discarded. A pending transaction publishes files but leaves the pin.
- **Resign without a pin:** refused unless this machine is the only enrolled machine; the governance diff is always printed, against the last committed manifest that matches its signed state when no pin exists.
- **Web panel:** the sync route accepts `expect_signer`; without it a first sync is a 403 Security error naming the fingerprint. Nothing is auto-accepted.

### 9. Hardening (round 8)
- **One pending decision for every consumer, bootstrap included:** a machine with no pin evaluates the latest governance transition (HEAD state against its predecessor in git history) with the same `check_transition` as the chain walk. If it is short of co-signatures, the machine pins the newest earlier state whose own transition was quorate (walking back through the history), reports `governance_pending` and applies nothing. Without usable git history for an epoch above 1 it refuses with a Security error.
- **`doctor` reports only while pending:** `doctor --fix` is refused, and the `adapters`, `keys`, `providers` and `provider_api_keys` checks are skipped, so nothing is planned, rendered or resolved from unaccepted governance. `sync` returns before planning.

### 10. Hardening (round 9): one authenticated transition rule for every consumer
**Rule.** Every (`fleet.state.json`, `muster.toml`, vault) triple taken from any source, HEAD or git history, is used only if its manifest recomputes the signed governance digest and its vault matches the signed digests. Any change to *who owns or controls a signing key* is a governance change and needs `k` co-signatures.

- **History is authenticated:** `history_states` keeps a historical (state, manifest) pair only if the state signature verifies and `governance_digest(manifest)` equals the state's signed digest; other pairs are skipped (never used for quorum, never pinned). Admin and signer sets come only from digest-verified manifests, so a manifest swapped in a later commit beside a re-serialized state no longer forges an admin set on a fresh machine. If nothing usable is left, bootstrap refuses with Security. Of several versions of one payload, the one with the most valid co-signatures is kept.
- **Narrow enrollment exemption:** a below-quorum transition is accepted only when it adds brand-new machines (new name, recipient and signing key, appearing nowhere in the prior governance) that carry their own `k` verified approvals and join exactly the member their approved request named, a non-admin one. Changing the machine list of an existing machine's owner, re-assigning or re-keying a machine, adding a machine to an admin member, or a machine under two members (refused outright) all need the full quorum. Votes use the prior state's member-to-machines mapping only.
- **Invalid state holds doctor:** like pending governance, an invalid signed state makes `doctor` report only: no plan, render, decryption, secret resolution or declared-key check, and `--fix` is refused.
- **Group readers bind every write:** `secret add` / `rotate` re-encrypt exactly to `team::group_recipients(manifest, group)` (readers plus recovery). A payload whose recipient set differs is refused with Security and `muster group rekey <group>` is suggested; new groups are checked against the same readers.
- **Missing manifest fails closed:** with a pin or `fleet.state.json` present, a missing or unparsable `muster.toml` is a Security error in `trust::guard` (doctor reports it as `fleet_state_invalid`), and an unnamed reveal no longer treats a missing manifest as "no policy".
- **Mutation lock:** `<state_dir>/mutation.lock`. Every mutating command and revocation recovery hold it; a concurrent command waits up to 5 s (`MUSTER_LOCK_WAIT_MS`) then fails with Operational "another muster command is running". Read-only commands only try-lock for recovery and leave a live stage alone when the lock is held. A scheduled `sync` that finds the lock busy skips instead of failing. (Round 11 replaced the create_new/pid/age scheme with a kernel advisory lock, see section 11.)
- **Admin vault rewrites:** with `k > 1`, an admin writing a vault group it does not read needs `k` co-signatures (pending otherwise). `trust cosign` lists the changed vault groups (names only) beside the governance diff, and checks the working tree against the payload digests before signing.

### 11. Hardening (round 11): structural closure of the quorum edge cases
- **Revocation is permanent.** `revoke` and `member rm` record the removed machine's signing key in the signed governance list `revoked_signing_keys` (omitted from the digest while empty, so older digests are unchanged), next to `revoked_recipients`. The verifier refuses any transition that adds a machine whose age recipient is in `revoked_recipients` or whose signing key is in `revoked_signing_keys` of the *prior* state, and such a machine never qualifies for the single-signature enrollment exemption (only the recovery key can sign past it, as for any break-glass change). Removing an entry from either list is an ordinary governance change and needs the full quorum, so un-revoking and re-adding are two quorate steps. Revoked recipients also no longer count as group readers (`team::group_recipients`, `ensure_reads_groups`).
- **Approvals are single-use.** The enrollment approval message (`muster enroll approval v1`) now also binds `state`, the payload hash of the fleet state the approver was looking at, and the verifier requires it to equal the `prev` hash of the transition that carries the evidence (together with the request nonce already bound). Approvals collected against an older state stop counting (the approver is told: `invalid_approval_ignored`), and public evidence copied from history cannot be replayed. Member-invitation approvals are *not* state-bound: they live in `muster.toml`, so each approval is itself a sealed epoch and a state binding would invalidate the previous ones. A replayed member activation yields a member with no machines, hence no signing vote; giving it a machine needs fresh state-bound enrollment approvals.
- **The vault rule holds on every transition.** `check_transition` computes the changed vault groups regardless of the governance branch. A non-admin signer must read every changed group; an admin signer rewriting a group it does not read needs `k` co-signatures when `k > 1`, even if the same transition bundles quorum-exempt governance edits (an invitation added or dropped, an approved enrollment). `pending_quorum` (the proposer's own pin) uses the same gate. A property test (`forged_ciphertext_property`) builds random mixes of those exempt edits plus unread-group rewrites signed by one admin and requires Pending or Security every time.
- **Bootstrap is one forward chain.** A machine without a pin evaluates the history as a contiguous sequence of transitions from genesis (epoch 1, no `prev`, signed by a key its own governance authorizes): a state supplies authority only if its own transition was accepted under the previous accepted state. A pending intermediate state ends the accepted prefix even when a later transition would pass in isolation; the machine pins the newest accepted state and reports the pending one. A history that does not reach epoch 1 (shallow or squashed) is refused. `--expect-signer` still names the signer of the current state (the whole chain is bound to it by the `prev` hash links); the genesis signer must additionally be authorized by the genesis manifest.
- **History follows `prev` links.** `history_chain` starts at the current state and, for each earlier epoch, takes only the state whose payload hash `prev` names, so a junk or forked state committed first at an epoch can no longer displace the real one. Versions of one payload are merged and all verifying co-signatures kept; only co-signers the predecessor authorized ever count toward the quorum, so padding with unrelated keys is harmless.
- **Kernel advisory lock.** `mutation.lock` is an OS advisory lock (`File::try_lock`, as in `sync.lock` and the secret-lease lock) held by an open file for the whole command or recovery. The kernel releases it when the holder dies, so there is no pid, start-time or age heuristic, a planted or leftover file with no holder is free, and the file is never unlinked (a guard cannot delete another holder's lock). A thread re-entering the lock holds nothing extra.
- **Rejecting a pending proposal.** `muster trust cosign --reject` (an admin of the last accepted state, or the recovery key) restores `fleet.state.json`, `muster.toml` and `vault/` to the commit whose state matches the local pin, verifies the result, commits and pushes the revert. Nothing unaccepted is kept and nothing accepted changes, so one admin is enough (a compromised admin can only delay a legitimate proposal, which its proposer can re-publish). The honest quorum can then revoke the proposer with the normal flow; the freeze no longer needs manual git repair.

### Known limitations
- **History-squash migration bypass:** the legacy-migration guard reads git history and remote configuration. Someone with write access to this machine's config repo can squash history and drop the remote so a pin-less machine migrates without the typed confirmation. That needs local write access, which is outside the A1 threat model; machines that already pinned refuse first.
- **Pending-proposal freeze (mitigated):** one compromised admin can still push a single-signed governance change and re-push it after each `trust cosign --reject`; peers refuse everything except `sync`, `doctor` and the reject until it is discarded. Serving read-only commands from the last accepted state while a proposal is pending, and competing proposals at the same epoch, are not implemented.
- **Legacy quorum evidence:** enrollment evidence signed before approvals were state-bound no longer verifies. A fleet with `required_approvals > 1` and such enrollments in its history needs a fresh checkpoint (`muster trust resign` by the recovery key) before a new machine can bootstrap through that history.
- **No web cosign:** the web panel cannot inspect a pending quorum or co-sign. `muster trust show` and `muster trust cosign` are CLI only.

## Status

Accepted and implemented in `hillclimb/secrets-hardening`.
