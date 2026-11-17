# Connect a machine to Muster Cloud

Muster Cloud is an optional hosted transport. The free CLI, team mode and your
own Git remote continue to work without a Cloud account. Cloud implementation
is in progress; this guide describes workflows tested against the development
service, not an available production deployment.

## Sign in and select a workspace

Open the service in your browser, sign in and create a workspace. Then run:

```sh
muster cloud login --endpoint https://cloud.example.com
muster cloud workspaces
```

Replace the example URL with your service's HTTPS address. Review the device
code in the browser and explicitly approve the CLI login. Use `--no-browser`
when you want to open the displayed address yourself.

Login authorizes your account to use Cloud. It does not approve a machine in
the fleet or transfer signing keys.

## Link and publish the first machine

Use an existing initialized fleet. If this machine has no Muster configuration,
initialize one first with `muster init --machine laptop`.

```sh
muster cloud link --workspace <workspace-id>
muster sync --push
```

Choose the identifier from `muster cloud workspaces`. Repeating the link command
for the same workspace preserves the machine identifier. The first machine of
the workspace owner may bootstrap an empty workspace; its signed fleet must
authorize its own signing key.

Cloud stores Git history and encrypted vault files. Configuration and vault
metadata can be visible to the service. Machine private keys and local account
tokens stay outside the synchronized repository.

## Verify a fleet on another machine

Sign in and link the other machine to the same workspace. Compare the signing
fingerprint with `muster trust show` on a trusted machine, then pull:

```sh
muster sync --pull --expect-signer <trusted-fingerprint>
```

For an exact state confirmation, use the payload hash from the trusted
machine instead:

```sh
muster sync --pull --expect-state <trusted-payload-hash>
```

The CLI verifies incoming history before applying it. A machine that already
accepted a fleet requires the exact state hash to replace that fleet with an
unrelated history. A signer name alone cannot discard its existing trust.

Linking and downloading do not grant permission to publish or decrypt secrets.
The fleet's local signed enrollment rules, including team quorum, govern that
permission. The complete Cloud enrollment and conflict-resolution CLI workflows
are still being implemented.

## Use the workspace console

The browser console lists linked machines, members, invitations, accepted
history and retained proposals. Diffs redact encrypted `.age` file contents.
A restore request creates a proposal; it does not apply an old state on the
server. An authorized client must review and sign a forward change under the
current fleet rules.

The console also shows the server's plan, limits and storage usage. Where billing
is configured, owners and admins can open checkout or the customer portal,
change a plan, and confirm cancellation. A checkout return URL never activates
a subscription: only the server's verified provider state does. Pull and export
remain available when payment blocks publication.

## Inspect or remove local access

```sh
muster --format json cloud status
muster --format json cloud unlink
```

Status reports the local endpoint, workspace and machine identifier without
contacting Cloud or revealing a token. A local link does not establish that the
service currently authorizes that machine.

Unlink removes the local link and its cached revision and publication state.
Repeating it succeeds, including when fleet verification fails. It preserves
the repository, fleet keys and account login; it does not cancel a subscription.

To remove the local account login, run:

```sh
muster --format json cloud logout
```

Logout attempts remote account-token revocation and reports `server_revoked`.
It removes the local login even if the service is unreachable. It does not
revoke fleet keys or cancel billing.

## Export a verified copy and leave Cloud

`muster cloud export` downloads the workspace's complete Git bundle with your
account session, verifies it against the service's export manifest (the bundle
hash, the accepted revision and epoch, and the bundle tip must all agree), and
writes it atomically with private permissions:

```sh
muster --format json cloud export --output fleet.bundle
```

The result reports the written `path`, the `revision`, and the full `manifest`
(revision, epoch, signer, sha256). Use `--workspace <id>` to export another
workspace your account can read — that works on a login-only machine and
after `cloud unlink`; only the default (linked) workspace needs the link. A
workspace the account cannot read is refused with the service's own status
and no file is written. Account and device tokens never appear in argv, URLs
or output. Export stays available to members regardless of plan status.

The verified bundle is your exit from Cloud. After `muster cloud unlink`, a
fresh machine restores the fleet from the bundle alone — no Cloud involved:

```sh
muster sync --pull --remote /path/to/fleet.bundle --expect-state <trusted-payload-hash>
muster enroll --name laptop
```

The join needs no prior `muster init`: it creates the config repo on `main`
(the branch fleet bundles publish) and verifies the complete signed history
against the exact `--expect-state` (or `--expect-signer`) read from
`muster trust show` on a machine you already trust before anything is
applied. From there the fleet works like any open-source team: enrollment
requests and approvals travel over plain Git remotes, and `muster sync`
applies and commits locally. If no original machine survives, the offline
recovery key `init` returned (or showed once) approves the fresh machine:

```sh
muster enroll --name laptop
cat recovery.key | muster approve laptop --recovery-key-stdin
```

Keep the exported bundle private — it contains the full configuration and
vault ciphertext history.

## Limits, retries and retention

The service limits request rates in memory and answers `429 rate_limited` with
a `Retry-After` header (seconds) when a budget is spent. Each budget refills
continuously over one minute:

| Budget | Default | Setting |
|---|---|---|
| Signed requests per machine | 60/min | `MUSTER_RATE_LIMIT_DEVICE_PER_MINUTE` |
| Link codes (`POST /v1/device/authorize`) per client address | 10/min | `MUSTER_RATE_LIMIT_LINK_CODES_PER_MINUTE` |
| Login polls (`POST /v1/device/token`) per client address | 60/min | `MUSTER_RATE_LIMIT_DEVICE_TOKEN_PER_MINUTE` |

`0` disables a budget. Background sync of a normal fleet makes a few signed
requests per run and stays far below the machine budget. The client address is
the TCP peer. Behind a reverse proxy, list the proxy in `MUSTER_TRUSTED_PROXIES`
(comma-separated addresses or CIDR networks, for example `127.0.0.1,10.0.0.0/8`):
only then is `X-Forwarded-For` read, and its rightmost hop that is not a listed
proxy is the client. Without it, every client behind the proxy shares one
address budget. IPv6 clients share a budget per `/64`.

A publication retried with the same `Idempotency-Key` and the same bytes gets
the original outcome again: the accepted revision, or the same held or conflict
proposal. The same key with other bytes is `422 idempotency_mismatch`. A
machine the accepted governance does not authorize gets `403` and the service
keeps nothing from its upload.

When a proposal is closed (rejected, resolved or superseded), the service
releases its retained upload automatically within about a minute: its bytes
stop counting as storage use, a push refused with `507 quota_exceeded` fits
again, and its bundle is no longer downloadable. Open proposals and the
accepted history are never released.

Bookkeeping is deleted automatically: replay nonces 15 minutes after use
(longer than a signature stays valid), idempotency records after 30 days
(longer than any client retries a request), and expired account tokens,
sessions and logins.

See [ADR 0006](../adr/0006-cloud-service.md) for the data and trust boundaries,
and the [deployment guide](../../deploy/README.md) for service configuration and
backup tools. Workspace deletion and automated backup retention remain under
development.
