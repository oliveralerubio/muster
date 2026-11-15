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

See [ADR 0006](../adr/0006-cloud-service.md) for the data and trust boundaries,
and the [deployment guide](../../deploy/README.md) for service configuration and
backup tools. Workspace deletion and automated backup retention remain under
development.
