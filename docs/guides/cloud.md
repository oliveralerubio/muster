# Inspect or remove a local Cloud link

Cloud implementation is in progress. This guide covers the available local
commands. Login, linking, hosted synchronization, and billing are not yet
documented as available workflows.

## Inspect the link

Run:

```sh
	muster cloud status --json
```

An unlinked machine reports `"status": "unlinked"`. A linked machine also reports
the Cloud endpoint, workspace identifier, and machine identifier. The command
does not contact Cloud or reveal the authentication token. A local link alone
does not establish that the service currently authorizes the machine.

## Remove the link

Run:

```sh
	muster cloud unlink --json
```

The command removes the local link and reports `"status": "unlinked"`. Repeating
the command succeeds. It works even when the fleet configuration cannot pass
verification, so a damaged fleet cannot prevent local account cleanup.

Unlinking removes local connection data. It does not cancel a subscription,
revoke fleet keys, or configure a customer-owned Git remote.

See [ADR 0006](../adr/0006-cloud-service.md) for the accepted Cloud design and its
data boundaries.
