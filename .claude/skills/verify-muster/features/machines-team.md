# Fleet machines, enrollments, and team members

Manage machines in the fleet (enrolled machines and pending enrollments) and team members (roles,
status, approvals and rejections).

## Sub-features

- List enrolled machines and pending enrollment requests (`GET /api/machines`, `muster machine list`).
- Approve or reject a machine's pending enrollment (`POST /api/approve`, `POST /api/reject`, `muster approve`, `muster reject`).
- Remove an enrolled machine (`POST /api/machine/rm`, `muster machine rm`).
- List team members and groups (`GET /api/members`, `GET /api/groups`, `muster member list`, `muster group list`).
- Invite a member (`POST /api/member/add`, `muster member add`).
- Approve or reject a pending member (`POST /api/member/approve`, `POST /api/member/reject`, `muster member approve`, `muster member reject`).

## How to get to it (user POV)

In the web panel, the **Machines** tab shows enrolled fleet members and pending enrollment requests
with "Approve" and "Reject" buttons. The **Team** tab shows members (with their role, status, groups,
and action buttons for pending members) and groups. CLI equivalents: `muster machine list`, `muster enroll`,
`muster approve`, `muster reject`, `muster member ...`.

## Driving it with muster-harness

```bash
$H launch $RUN
# 1. Pending machine enrollment
M_KEY=$(HOME=$(mktemp -d) ./target/debug/muster --yes init 2>/dev/null | jq -r .machine.recipient)
mkdir -p /tmp/muster-verify/$RUN/home/.config/muster/enroll
cat << TOML > /tmp/muster-verify/$RUN/home/.config/muster/enroll/box-a.toml
name = "box-a"
recipient = "$M_KEY"
platform = "linux-x86_64"
muster_version = "0.1.0"
agents = ["claude"]
created_at = "2026-10-15T12:00:00Z"
TOML

$H api $RUN GET machines                               # shows box-a in result.pending
$H screenshot $RUN machines machines-pending.png
$H api $RUN POST approve '{"name":"box-a","confirm":true}'
$H api $RUN GET machines                               # box-a is now in result.machines (status active)

# 2. Team members: add, approve, reject
$H api $RUN POST member/add '{"name":"alice","confirm":true}'
$H api $RUN POST member/add '{"name":"bob","confirm":true}'
$H api $RUN GET members                                # alice and bob are pending
$H screenshot $RUN team team-pending.png
$H api $RUN POST member/approve '{"name":"alice","confirm":true}'
$H api $RUN POST member/reject '{"name":"bob","confirm":true}'
$H api $RUN GET members                                # alice is active; bob is removed
```

Observable end state: `GET /api/machines` shows the approved machine under `machines` and pending is empty;
`GET /api/members` shows `alice` as `active` and `bob` is absent.

## Gotchas

- Machine enrollment requests are stored in `<config_dir>/enroll/<name>.toml`.
- An approver must have an admin role or provide the break-glass recovery key.
- Rejection discards recorded approvals and removes the request file; it is not a revocation (the machine or member can apply afresh).
- Quorum rules apply when multiple admins exist (`[team] required_approvals`).
