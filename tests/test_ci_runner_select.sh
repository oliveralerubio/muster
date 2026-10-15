#!/usr/bin/env bash
# Test suite for scripts/ci-runner-select.sh.
#
# Runs without network access: `gh` is mocked by a fake script placed first on
# PATH. The fake serves canned responses from FAKE_* env vars and logs every
# invocation to $FAKE_GH_LOG so tests can assert on `variable set` and
# `workflow run` calls.
#
# Usage: bash tests/test_ci_runner_select.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/ci-runner-select.sh"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAKE_BIN="$WORK/bin"
LOG="$WORK/gh.log"
mkdir -p "$FAKE_BIN"

PASS=0
FAIL=0

ok() {
    PASS=$((PASS + 1))
    printf 'PASS - %s\n' "$1"
}

ko() {
    FAIL=$((FAIL + 1))
    printf 'FAIL - %s\n' "$1" >&2
}

assert_eq() { # description actual expected
    if [ "$2" = "$3" ]; then
        ok "$1"
    else
        ko "$1 (expected: $3, got: $2)"
    fi
}

assert_contains() { # description haystack needle
    case "$2" in
        *"$3"*) ok "$1" ;;
        *) ko "$1 (does not contain: $3; got: $2)" ;;
    esac
}

assert_not_contains() { # description haystack needle
    case "$2" in
        *"$3"*) ko "$1 (unexpectedly contains: $3)" ;;
        *) ok "$1" ;;
    esac
}

# ------------------------------------------------------------------- mocks --

cat >"$FAKE_BIN/gh" <<'MOCK'
#!/usr/bin/env bash
# fake gh: canned responses from FAKE_* env vars; every call is logged
set -u
: "${FAKE_GH_LOG:?}"
printf '%s\n' "$*" >>"$FAKE_GH_LOG"
case "${1:-} ${2:-}" in
    "api repos/"*"/actions/variables/CI_RUNNER")
        if [ -n "${FAKE_VAR_CURRENT+x}" ]; then
            printf '%s\n' "$FAKE_VAR_CURRENT"
        else
            echo "gh: Not Found (HTTP 404)" >&2
            exit 1
        fi
        ;;
    "api repos/"*"/actions/runs/"*"/jobs"*)
        if [ "${FAKE_JOBS_FAIL:-0}" = "1" ]; then
            echo "gh: network error" >&2
            exit 1
        fi
        id="${2#*/actions/runs/}"
        id="${id%%/*}"
        per_run="FAKE_JOBS_JSON_$id"
        printf '%s\n' "${!per_run:-${FAKE_JOBS_JSON:-{\"jobs\":[]\}}}"
        ;;
    "api repos/"*)
        if [ "${FAKE_RUNNERS_FAIL:-0}" = "1" ]; then
            echo "gh: network error" >&2
            exit 1
        fi
        printf '%s\n' "${FAKE_RUNNERS_JSON:-{\"total_count\":0,\"runners\":[]\}}"
        ;;
    "run list")
        case "$*" in
            *"--workflow ci.yml"*)
                if [ "${FAKE_CI_RUNS_FAIL:-0}" = "1" ]; then
                    echo "gh: network error" >&2
                    exit 1
                fi
                # Honour --status <conclusion> and --limit like the real gh does.
                want="$(sed -n 's/.*--status \([a-z_]*\).*/\1/p' <<<"$*")"
                lim="$(sed -n 's/.*--limit \([0-9]*\).*/\1/p' <<<"$*")"
                jq -c --arg want "$want" --argjson lim "${lim:-20}" \
                    '[.[] | select($want == "" or .conclusion == $want)] | .[:$lim]' <<<"${FAKE_CI_RUNS_JSON:-[]}"
                ;;
            *)
                if [ "${FAKE_RUNS_FAIL:-0}" = "1" ]; then
                    echo "gh: network error" >&2
                    exit 1
                fi
                printf '%s\n' "${FAKE_RUNS_JSON:-[]}"
                ;;
        esac
        ;;
    "variable set" | "workflow run")
        ;;
    *)
        echo "fake gh: unexpected call: $*" >&2
        exit 2
        ;;
esac
MOCK
chmod +x "$FAKE_BIN/gh"

# ------------------------------------------------------------------ helpers --

SELF_HOSTED='["self-hosted","fast-runner"]'
HOSTED='["ubuntu-latest"]'

RUNNER_ONLINE='{"total_count":1,"runners":[{"id":1,"name":"omarchy-muster","status":"online"}]}'
RUNNER_OFFLINE='{"total_count":1,"runners":[{"id":1,"name":"omarchy-muster","status":"offline"}]}'

iso_ago() { # seconds-ago -> ISO-8601 UTC timestamp
    date -u -d "$1 seconds ago" +"%Y-%m-%dT%H:%M:%SZ"
}

run_json() { # status conclusion age-seconds -> gh run list JSON (updatedAt == createdAt)
    printf '[{"status":"%s","conclusion":"%s","createdAt":"%s","updatedAt":"%s"}]' \
        "$1" "$2" "$(iso_ago "$3")" "$(iso_ago "$3")"
}

ci_obj() { # id conclusion created-ago updated-ago -> one ci.yml run object
    printf '{"databaseId":%s,"status":"completed","conclusion":"%s","createdAt":"%s","updatedAt":"%s"}' \
        "$1" "$2" "$(iso_ago "$3")" "$(iso_ago "$4")"
}

ci_json() { # status conclusion age-seconds -> ci.yml run list JSON (id 42, updatedAt == createdAt)
    printf '[%s]' "$(ci_obj 42 "$2" "$3" "$3")"
}

# Jobs payloads for run 42
JOBS_BILLING='{"jobs":[{"labels":["ubuntu-latest"],"conclusion":"failure","runner_id":0,"steps":[]}]}'
JOBS_REAL_FAIL='{"jobs":[{"labels":["ubuntu-latest"],"conclusion":"failure","runner_id":7,"steps":[{"name":"Tests","conclusion":"failure"}]}]}'
JOBS_SELF_HOSTED='{"jobs":[{"labels":["self-hosted","fast-runner"],"conclusion":"failure","runner_id":0,"steps":[]}]}'

# run_select [VAR=value ...] ; sets $status, $output, $calls.
# Starts every case from a clean slate (no inherited FAKE_*).
run_select() {
    : >"$LOG"
    set +e
    output="$(
        env -u FAKE_RUNNERS_JSON -u FAKE_RUNNERS_FAIL -u FAKE_RUNS_JSON -u FAKE_RUNS_FAIL -u FAKE_CI_RUNS_JSON -u FAKE_CI_RUNS_FAIL -u FAKE_JOBS_JSON -u FAKE_JOBS_JSON_41 -u FAKE_JOBS_JSON_42 -u FAKE_JOBS_FAIL -u FAKE_VAR_CURRENT \
            PATH="$FAKE_BIN:$PATH" FAKE_GH_LOG="$LOG" "$@" \
            bash "$SCRIPT" 2>/dev/null
    )"
    status=$?
    set -e
    calls="$(cat "$LOG")"
}

# ------------------------------------------------------------------- tests --

echo "# runner online"
run_select FAKE_RUNNERS_JSON="$RUNNER_ONLINE"
assert_eq "online: exits 0" "$status" "0"
assert_contains "online: sets self-hosted labels" "$calls" "variable set CI_RUNNER --repo oliveralerubio/muster --body $SELF_HOSTED"
assert_not_contains "online: does not trigger a probe" "$calls" "workflow run"
assert_eq "online: summary line" "$output" \
    "CI_RUNNER=$SELF_HOSTED runner=online probe=none ci=none changed=yes probed=no"

echo "# runner offline, recent successful probe"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json completed success 600)"
assert_eq "offline+success: exits 0" "$status" "0"
assert_contains "offline+success: sets ubuntu-latest" "$calls" "--body $HOSTED"
assert_not_contains "offline+success: no new probe yet" "$calls" "workflow run"
assert_eq "offline+success: summary line" "$output" \
    "CI_RUNNER=$HOSTED runner=offline probe=success ci=ok changed=yes probed=no"

echo "# runner offline, failed probe"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json completed failure 600)"
assert_eq "offline+failure: exits 0" "$status" "0"
assert_contains "offline+failure: sets none" "$calls" "--body none"
assert_not_contains "offline+failure: recent failure is not re-probed" "$calls" "workflow run"
assert_eq "offline+failure: summary line" "$output" \
    "CI_RUNNER=none runner=offline probe=failure ci=ok changed=yes probed=no"

echo "# runner offline, no probe yet"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON='[]'
assert_eq "offline+no probe: exits 0" "$status" "0"
assert_contains "offline+no probe: triggers a probe on main" "$calls" "workflow run hosted-probe.yml --repo oliveralerubio/muster --ref main"
assert_contains "offline+no probe: sets none" "$calls" "--body none"
assert_eq "offline+no probe: summary line" "$output" \
    "CI_RUNNER=none runner=offline probe=none ci=ok changed=yes probed=yes"

echo "# runner offline, probe aging (interval 15m, max age 30m)"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json completed success 1200)"
assert_eq "aging probe (20m): still usable, re-probe requested" "$output" \
    "CI_RUNNER=$HOSTED runner=offline probe=success ci=ok changed=yes probed=yes"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json completed success 5000)"
assert_contains "old probe (83m): triggers a new probe" "$calls" "workflow run hosted-probe.yml"
assert_eq "old probe (83m): no longer trusted (this was the 2h stale window Sol flagged)" "$output" \
    "CI_RUNNER=none runner=offline probe=stale ci=ok changed=yes probed=yes"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json completed success 2400)"
assert_eq "expired probe (40m): none" "$output" \
    "CI_RUNNER=none runner=offline probe=stale ci=ok changed=yes probed=yes"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json completed success 600)"
assert_not_contains "fresh probe (10m): no re-probe" "$calls" "workflow run"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json completed success 600)" \
    PROBE_INTERVAL=300 PROBE_MAX_AGE=900
assert_contains "env override: PROBE_INTERVAL honoured" "$calls" "workflow run hosted-probe.yml"

echo "# runner offline, probe in progress"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json in_progress "" 60)"
assert_eq "offline+in_progress: exits 0" "$status" "0"
assert_not_contains "offline+in_progress: no new probe" "$calls" "workflow run"
assert_eq "offline+in_progress: summary line" "$output" \
    "CI_RUNNER=none runner=offline probe=pending ci=ok changed=yes probed=no"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json queued "" 60)"
assert_not_contains "offline+queued: no new probe" "$calls" "workflow run"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json queued "" 30000)"
assert_contains "offline+stuck queued probe (8h): re-probes" "$calls" "workflow run hosted-probe.yml"

echo "# pending probe does not hide the last good result"
PENDING_AND_OK="[$(run_json in_progress "" 30 | sed 's/^\[//;s/\]$//'),$(run_json completed success 600 | sed 's/^\[//;s/\]$//')]"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$PENDING_AND_OK"
assert_eq "pending + earlier success: hosted, no extra probe" "$output" \
    "CI_RUNNER=$HOSTED runner=offline probe=success ci=ok changed=yes probed=no"

echo "# cancelled probe is not evidence"
CANCELLED_AND_OK="[$(run_json completed cancelled 30 | sed 's/^\[//;s/\]$//'),$(run_json completed success 600 | sed 's/^\[//;s/\]$//')]"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$CANCELLED_AND_OK"
assert_contains "cancelled then success: hosted" "$output" "CI_RUNNER=$HOSTED"

echo "# real CI feedback (hosted infrastructure failures)"
GOOD_PROBE="$(run_json completed success 120)"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$GOOD_PROBE" \
    FAKE_CI_RUNS_JSON="$(ci_json completed failure 60)" FAKE_JOBS_JSON="$JOBS_BILLING"
assert_eq "billing failure on hosted CI newer than good probe: none immediately" "$output" \
    "CI_RUNNER=none runner=offline probe=success ci=infra-failure changed=yes probed=no"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$GOOD_PROBE" \
    FAKE_CI_RUNS_JSON="$(ci_json completed startup_failure 60)"
assert_contains "startup_failure on CI: none" "$output" "CI_RUNNER=none"
assert_contains "startup_failure on CI: flagged" "$output" "ci=infra-failure"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$GOOD_PROBE" \
    FAKE_CI_RUNS_JSON="$(ci_json completed failure 600)" FAKE_JOBS_JSON="$JOBS_BILLING"
assert_eq "infra failure OLDER than the good probe: probe proves recovery, hosted" "$output" \
    "CI_RUNNER=$HOSTED runner=offline probe=success ci=ok changed=yes probed=no"

echo "# regression: a later skipped/success CI run must not hide a billing failure"
# Billing failure 600s ago (run 41), then an all-skipped run concluded success 60s ago (run 42).
# Probe is 1500s old: still inside max age, but older than the failure.
OLD_PROBE="$(run_json completed success 1500)"
TWO_RUNS="[$(ci_obj 42 success 70 60),$(ci_obj 41 failure 650 600)]"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$OLD_PROBE" \
    FAKE_CI_RUNS_JSON="$TWO_RUNS" FAKE_JOBS_JSON_41="$JOBS_BILLING" FAKE_VAR_CURRENT="none"
assert_eq "success run after billing failure + older probe: stays none" "$output" \
    "CI_RUNNER=none runner=offline probe=success ci=infra-failure changed=no probed=yes"
assert_not_contains "later success run: variable not flipped to hosted" "$calls" "--body $HOSTED"
assert_not_contains "later success run: only the failed run costs a jobs call" "$calls" "runs/42/jobs"

echo "# regression: run created before the probe but failing after it"
# Run 42 was created 1500s ago (before the 900s-old probe) and hit billing 300s ago (after it).
LATE_FAIL="[$(ci_obj 42 failure 1500 300)]"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json completed success 900)" \
    FAKE_CI_RUNS_JSON="$LATE_FAIL" FAKE_JOBS_JSON="$JOBS_BILLING"
assert_eq "failure finished after the probe (created before it): none" "$output" \
    "CI_RUNNER=none runner=offline probe=success ci=infra-failure changed=yes probed=no"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json completed success 120)" \
    FAKE_CI_RUNS_JSON="$LATE_FAIL" FAKE_JOBS_JSON="$JOBS_BILLING"
assert_contains "probe finished after the failure: recovered, hosted" "$output" "CI_RUNNER=$HOSTED"

SAME_TS="$(iso_ago 300)"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" \
    FAKE_RUNS_JSON="[{\"status\":\"completed\",\"conclusion\":\"success\",\"createdAt\":\"$SAME_TS\",\"updatedAt\":\"$SAME_TS\"}]" \
    FAKE_CI_RUNS_JSON="[{\"databaseId\":42,\"status\":\"completed\",\"conclusion\":\"failure\",\"createdAt\":\"$SAME_TS\",\"updatedAt\":\"$SAME_TS\"}]" \
    FAKE_JOBS_JSON="$JOBS_BILLING"
assert_contains "probe finished at the same instant as the failure: still blocked (strictly newer)" "$output" "ci=infra-failure"

echo "# regression: 50 later skipped/success runs never displace the failure"
MANY="["
for i in $(seq 1 50); do MANY+="$(ci_obj $((100 + i)) skipped $((600 - i)) $((590 - i))),"; done
MANY+="$(ci_obj 41 failure 650 600)]"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$OLD_PROBE" \
    FAKE_CI_RUNS_JSON="$MANY" FAKE_JOBS_JSON_41="$JOBS_BILLING" FAKE_VAR_CURRENT="none"
assert_contains "50 skipped runs after a billing failure: still blocked" "$output" "CI_RUNNER=none runner=offline probe=success ci=infra-failure"

echo "# regression: many later ordinary build failures never hide the billing failure"
# Probe finished 1500s ago; billing failure finished 600s ago; 10 ordinary red builds after it.
REDS="["
for i in $(seq 1 10); do REDS+="$(ci_obj $((200 + i)) failure $((400 - i)) $((300 - i))),"; done
REDS+="$(ci_obj 41 failure 650 600)]"
JOBS_RED='{"jobs":[{"labels":["ubuntu-latest"],"conclusion":"failure","runner_id":7,"steps":[{"name":"Tests"}]}]}'
export FAKE_JOBS_JSON_REDS="$JOBS_RED"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$OLD_PROBE" \
    FAKE_CI_RUNS_JSON="$REDS" FAKE_JOBS_JSON="$JOBS_RED" FAKE_JOBS_JSON_41="$JOBS_BILLING" FAKE_VAR_CURRENT="none"
unset FAKE_JOBS_JSON_REDS
assert_eq "10 red builds after billing failure: still blocked" "$output" \
    "CI_RUNNER=none runner=offline probe=success ci=infra-failure changed=no probed=yes"

echo "# truncated failure list: 100 failures all newer than the probe cannot be proven complete"
HUNDRED="["
for i in $(seq 1 100); do HUNDRED+="$(ci_obj $((300 + i)) failure $((400 - i / 10)) $((300 - i / 10))),"; done
HUNDRED="${HUNDRED%,}]"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$OLD_PROBE" \
    FAKE_CI_RUNS_JSON="$HUNDRED" FAKE_JOBS_JSON="$JOBS_REAL_FAIL" FAKE_VAR_CURRENT="none"
assert_eq "100 failures newer than the probe: truncated, none" "$output" \
    "CI_RUNNER=none runner=offline probe=success ci=truncated changed=no probed=yes"
assert_not_contains "truncated: no jobs calls" "$calls" "/jobs"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$OLD_PROBE" \
    FAKE_CI_RUNS_JSON="$(jq -c '.[0:99] + [{"databaseId":500,"status":"completed","conclusion":"failure","createdAt":"2000-01-01T00:00:00Z","updatedAt":"2000-01-01T00:10:00Z"}]' <<<"$HUNDRED")" \
    FAKE_JOBS_JSON="$JOBS_REAL_FAIL"
assert_contains "full page that reaches back past the probe: covered, hosted" "$output" "CI_RUNNER=$HOSTED"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$OLD_PROBE" \
    FAKE_CI_RUNS_JSON="$(jq -c '.[0:99]' <<<"$HUNDRED")" FAKE_JOBS_JSON="$JOBS_REAL_FAIL"
assert_contains "99 failures (under the limit): fully covered, hosted" "$output" "CI_RUNNER=$HOSTED"

echo "# failures older than the probe are superseded and not inspected"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json completed success 120)" \
    FAKE_CI_RUNS_JSON="[$(ci_obj 41 failure 650 600)]" FAKE_JOBS_JSON="$JOBS_BILLING"
assert_contains "old failure before the probe: hosted" "$output" "CI_RUNNER=$HOSTED"
assert_not_contains "old failure before the probe: no jobs call" "$calls" "/jobs"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json completed failure 120)" \
    FAKE_CI_RUNS_JSON="[$(ci_obj 41 failure 650 600)]" FAKE_JOBS_JSON="$JOBS_BILLING"
assert_not_contains "no good probe: no jobs calls at all" "$calls" "/jobs"

echo "# most recent infra failure wins; ordinary red builds are skipped over"
RED_AFTER_BILLING="[$(ci_obj 42 failure 70 60),$(ci_obj 41 failure 650 600)]"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$OLD_PROBE" \
    FAKE_CI_RUNS_JSON="$RED_AFTER_BILLING" FAKE_JOBS_JSON_42="$JOBS_REAL_FAIL" FAKE_JOBS_JSON_41="$JOBS_BILLING"
assert_contains "red build after billing failure does not clear the block" "$output" "ci=infra-failure"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$GOOD_PROBE" \
    FAKE_CI_RUNS_JSON="$(ci_json completed failure 60)" FAKE_JOBS_JSON="$JOBS_REAL_FAIL"
assert_eq "ordinary red build (test failure): hosted stays enabled" "$output" \
    "CI_RUNNER=$HOSTED runner=offline probe=success ci=ok changed=yes probed=no"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$GOOD_PROBE" \
    FAKE_CI_RUNS_JSON="$(ci_json completed failure 60)" FAKE_JOBS_JSON="$JOBS_SELF_HOSTED"
assert_eq "empty-step failure on a self-hosted job is not a hosted signal" "$output" \
    "CI_RUNNER=$HOSTED runner=offline probe=success ci=ok changed=yes probed=no"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$GOOD_PROBE" \
    FAKE_CI_RUNS_JSON="$(ci_json completed cancelled 60)"
assert_contains "cancelled CI run is ignored" "$output" "CI_RUNNER=$HOSTED"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$GOOD_PROBE" \
    FAKE_CI_RUNS_JSON="$(ci_json completed success 60)"
assert_contains "successful CI run: hosted" "$output" "CI_RUNNER=$HOSTED"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$GOOD_PROBE" \
    FAKE_CI_RUNS_JSON="$(ci_json completed failure 60)" FAKE_JOBS_FAIL=1
assert_eq "cannot read jobs of failed CI run: fail safe to none" "$output" \
    "CI_RUNNER=none runner=offline probe=success ci=unknown changed=yes probed=no"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$GOOD_PROBE" FAKE_CI_RUNS_FAIL=1
assert_eq "cannot list CI runs: fail safe to none" "$output" \
    "CI_RUNNER=none runner=offline probe=success ci=unknown changed=yes probed=no"

run_select FAKE_RUNNERS_JSON="$RUNNER_ONLINE" \
    FAKE_CI_RUNS_JSON="$(ci_json completed startup_failure 60)"
assert_contains "runner online: CI feedback ignored, self-hosted wins" "$output" "CI_RUNNER=$SELF_HOSTED"
assert_not_contains "runner online: CI runs not even queried" "$calls" "--workflow ci.yml"

echo "# unchanged value"
run_select FAKE_RUNNERS_JSON="$RUNNER_ONLINE" FAKE_VAR_CURRENT="$SELF_HOSTED"
assert_eq "unchanged: exits 0" "$status" "0"
assert_not_contains "unchanged: no variable set" "$calls" "variable set"
assert_eq "unchanged: summary line" "$output" \
    "CI_RUNNER=$SELF_HOSTED runner=online probe=none ci=none changed=no probed=no"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_JSON="$(run_json completed failure 600)" \
    FAKE_VAR_CURRENT="none"
assert_not_contains "unchanged none: no variable set" "$calls" "variable set"

echo "# changed value"
run_select FAKE_RUNNERS_JSON="$RUNNER_ONLINE" FAKE_VAR_CURRENT="none"
assert_contains "changed: variable updated" "$calls" "variable set CI_RUNNER"

echo "# runners API failure (fail safe: publish none, then exit non-zero)"
run_select FAKE_RUNNERS_FAIL=1 FAKE_VAR_CURRENT="$SELF_HOSTED"
if [ "$status" -ne 0 ]; then
    ok "api failure: exits non-zero"
else
    ko "api failure: exited 0"
fi
assert_contains "api failure: sets none instead of keeping self-hosted" "$calls" "variable set CI_RUNNER --repo oliveralerubio/muster --body none"
assert_not_contains "api failure: never keeps self-hosted" "$calls" "--body $SELF_HOSTED"
assert_not_contains "api failure: no probe" "$calls" "workflow run"
assert_eq "api failure: summary line" "$output" \
    "CI_RUNNER=none runner=error probe=none ci=none changed=yes probed=no"

run_select FAKE_RUNNERS_FAIL=1 FAKE_VAR_CURRENT="none"
assert_not_contains "api failure, already none: no variable set" "$calls" "variable set"

echo "# run list failure (runner offline: fail safe to none)"
run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_FAIL=1 FAKE_VAR_CURRENT="$SELF_HOSTED"
assert_eq "run list failure: exits 0" "$status" "0"
assert_contains "run list failure: sets none instead of keeping self-hosted" "$calls" "variable set CI_RUNNER --repo oliveralerubio/muster --body none"
assert_not_contains "run list failure: never keeps self-hosted" "$calls" "--body $SELF_HOSTED"
assert_not_contains "run list failure: no probe" "$calls" "workflow run"
assert_eq "run list failure: summary line" "$output" \
    "CI_RUNNER=none runner=offline probe=none ci=ok changed=yes probed=no"

run_select FAKE_RUNNERS_JSON="$RUNNER_OFFLINE" FAKE_RUNS_FAIL=1 FAKE_VAR_CURRENT="none"
assert_eq "run list failure, already none: exits 0" "$status" "0"
assert_not_contains "run list failure, already none: no variable set" "$calls" "variable set"

echo "# variable unset (404)"
run_select FAKE_RUNNERS_JSON="$RUNNER_ONLINE"
assert_eq "unset variable: exits 0" "$status" "0"
assert_contains "unset variable: gets created" "$calls" "variable set CI_RUNNER"
assert_contains "unset variable: read via api" "$calls" "api repos/oliveralerubio/muster/actions/variables/CI_RUNNER --jq .value"

# ----------------------------------------------------------------- summary --

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
