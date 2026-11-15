# ADR 0006: Optional Cloud transport with local fleet authority

Status: Accepted design; implementation in progress.

Muster Cloud is an optional, self-service subscription for hosted synchronization
and workspace administration. The free CLI, team mode, vault, and customer-owned
Git transport remain available. Cloud runs as a separate service because the
existing loopback web panel acts with one machine's local privileges.

## Preserve signed history across the hosted transport

Cloud exchanges Git bundles over an authenticated HTTP API. Clients keep their
local repository and verify incoming history before applying configuration.
[ADR 0005](0005-signed-fleet-state.md) requires intermediate signed states, so a
snapshot-only service cannot preserve the existing trust model. Exposing a Git
remote directly would still require a separate API for permissions, proposals,
and machine receipts.

PostgreSQL records the accepted revision and its complete public trust anchor.
The service stores immutable Git objects and candidate references before
committing that record. It serves the revision recorded in PostgreSQL. Any
convenience branch is derived state that can be repaired after a crash. This
avoids treating a Git ref update and a database transaction as one atomic write.

A publication must name its base revision. A stale publication becomes a retained
proposal instead of overwriting the accepted revision. Pending governance remains
a proposal until it satisfies the signed fleet rules. Restoration creates a new
revision under current governance, rather than reviving an old authorization.

## Keep account permissions separate from fleet signatures

The Cloud account controls service access and billing. It does not hold machine
signing keys or the recovery key. A Cloud role never substitutes for a signed
fleet approval. The server verifies history against an explicit public anchor
without creating a local machine identity or implicitly trusting a signer.

Machines sign transport requests with the workspace, method, resource, body
digest, base revision, idempotency key, timestamp, and nonce. Receipts describe
what a machine reports applying. They do not prove agent behavior or erase
credentials already delivered to a machine.

## State the data and exit boundaries

Vault values remain encrypted by the client. Configuration and vault metadata
can be visible to the service, so this design does not promise encryption of all
workspace content. Machine private keys, resolved local generations, and agent
login credentials are excluded from synchronization.

Cloud retains the authenticated history needed to verify the fleet throughout
the workspace's lifetime. A plan's visible history window does not imply that
older Git objects are purged. Cancellation preserves paid access until the
period ends, followed by an export window. Export includes the complete history
needed by the free CLI. Account recovery does not recover lost fleet keys.

## Verification boundary

Acceptance uses the public Cloud API, the installed CLI on two isolated machines,
and the browser. External identity and billing providers use documented test
environments or protocol fixtures at their network boundary. Passing those tests
does not establish production credentials, banking settlement, or deployment.
