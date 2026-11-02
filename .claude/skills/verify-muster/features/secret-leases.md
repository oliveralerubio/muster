# Runtime secret leases and human approvals

When an agent (identified via MCP or CLI `--agent`) asks to reveal a secret whose policy is
`approval = "each"` in `[secrets."<reference>"]`, muster refuses, records a pending request in
`secret-leases.json` (state dir) and tells the human the request id. A human approves (`muster
secret approve <id> [--ttl 15m]`, TTL max 24h) from a real terminal or a confirmed web session —
`--yes` alone is NOT enough without a TTY. Approval grants a time-boxed lease; a later reveal within
the lease succeeds and is audited. `muster secret deny <id>` removes the request; `muster secret
revoke <id>|--all` kills leases early.

## Sub-features

- Request creation: first agent reveal attempt fails with diagnostic `secret_approval_required`
  (suggestion: `muster secret approve <id>`), writes a `secret.request` history line and the pending
  request (repeated attempts reuse the same id).
- Listing: `muster secret requests`, `muster secret leases`; API `GET /api/secret/requests`,
  `GET /api/secret/leases`.
- Approve / deny: `POST /api/secret/approve` `{id, ttl?, confirm:true}` (the web session with
  `confirm` counts as a human), `POST /api/secret/deny`.
- Revoke: `POST /api/secret/revoke` `{id}` or `{"all": true}`; after a revoke, further reveals are
  blocked again (the lock covers policy check and audited reveal).
- Panel: the "Secrets & approvals" tab lists pending requests (Approve/Deny buttons) and active
  leases (Revoke button).

## How to get to it (user POV)

Declare the policy once in muster.toml:

```toml
[secrets."native://verify/api"]
allow = ["claude"]
approval = "each"
approval_ttl = "15m"
```

The agent's reveal fails; the human opens the panel (Secrets tab) or runs `muster secret requests`,
approves, and the agent retries within the TTL. The Secrets tab shows both queues on one screen.

## Driving it with muster-harness

```bash
$H launch $RUN
$H tree $RUN before
$H cli $RUN --stdin 'verify-dummy-123' -- --yes secret add verify/api --value-stdin --allow claude
# Set the per-reference approval policy (the sandbox copy, never your real one):
#   append to /tmp/muster-verify/$RUN/home/.config/muster/muster.toml, at TOP LEVEL
#   (before any [table], e.g. right after `version = 1`):
#     [secrets."native://verify/api"]
#     allow = ["claude"]
#     approval = "each"
#     approval_ttl = "15m"
# And to allow the MCP path at all:
#     [mcp_server]
#     reveal = ["native://verify/api"]
$H api $RUN GET secret/requests           # initially []
$H mcp $RUN tools-call secret_reveal '{"reference":"native://verify/api"}'   # expect error secret_approval_required
$H api $RUN GET secret/requests           # now one entry with id, reference, agent, source "mcp"
$H api $RUN POST secret/approve '{"id":"<id>","ttl":"15m","confirm":true}'
$H api $RUN GET secret/leases             # one lease with expires_at ≈ now+900
$H mcp $RUN tools-call secret_reveal '{"reference":"native://verify/api"}'   # now succeeds; harness exits 0, STDOUT SHOWS the value
$H leakcheck $RUN verify-dummy-123        # value must not be in sandbox files
# Stdout shows the revealed value so the agent can see it; evidence never does:
grep -R 'verify-dummy-123' /tmp/muster-verify-evidence/$RUN/              # expect no matches (exit 1)
$H api $RUN POST secret/revoke '{"id":"<lease-id>","confirm":true}'
$H mcp $RUN tools-call secret_reveal '{"reference":"native://verify/api"}'   # blocked again, new request
$H api $RUN POST secret/deny '{"id":"<new-id>","confirm":true}'
tail -n 6 /tmp/muster-verify/$RUN/home/.local/state/muster/history.jsonl   # secret.request/reveal/deny lines
```

Observable end state that proves it: `secret-leases.json` gains the request then the lease;
`history.jsonl` records `secret.request`, `secret.reveal` (actor = the MCP client name), `secret.deny`
or `secret.revoke` lines with the reference and never the value; approve without TTY/confirm fails
(`--yes` alone exits nonzero); a revoked lease blocks the next reveal; leakcheck stays CLEAN and the
the revealed value appears on the harness's STDOUT only — evidence files never contain it: the
harness withholds the `secret_reveal` value payload (and any reveal API response body) from evidence,
while error diagnostics (`secret_approval_required` code, message, hint, request id) ARE kept in
evidence so the approval flow can be followed.
Note the first blocked reveal exits the harness nonzero (the MCP reply carries the
`secret_approval_required` error), while the approved reveal exits 0.

## Gotchas

- Approval is deliberately NOT possible with `--yes` alone and no TTY — the CLI path needs a real
  terminal; only `confirm: true` on the web API counts as human approval.
- TTL parse: positive `Ns|Nm|Nh`, 1 second..24 hours; anything else is a usage error.
- The lease is per (reference, agent): a lease for claude does not cover codex.
- MCP `secret_reveal` forces `--reveal`; without an active lease the first call always creates the
  request and fails — that failure is the feature, not a harness bug.
- Three MCP-only gates come BEFORE the approval flow, in order: (1) the reference must be listed in
  `[mcp_server] reveal` in muster.toml; (2) the client must identify as a known agent — the harness
  sends `initialize` with `clientInfo.name` containing "claude" so `client_agent` maps it to
  `claude`; an unidentified client is refused outright (no request is created); (3) the reference's
  `[secrets...] allow` must grant that agent.
- The policy table `[secrets."..."]` is top-level in muster.toml: after `version = 1`, before any
  other table; and `[secrets."..."]` keys must be unique — `secret add --allow` already writes one
  for the same reference, so edit the existing table instead of appending a duplicate.
- `secret_reveal` SUCCESS prints the plaintext on stdout (that is the point: the agent needs the
  value); the evidence file records the redacted form. A FAILED reveal keeps its diagnostics in
  evidence. Use dummy values only.
