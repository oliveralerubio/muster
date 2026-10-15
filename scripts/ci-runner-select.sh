#!/usr/bin/env bash
# Decides where CI runs and publishes the decision in the repo variable CI_RUNNER,
# which .github/workflows/ci.yml reads at queue time:
#   - self-hosted runner online          -> ["self-hosted","fast-runner"]
#   - else hosted minutes proven working -> ["ubuntu-latest"]
#   - else                               -> none (CI jobs are skipped)
#
# While the self-hosted runner is offline, hosted runners are used ONLY if both
# signals agree (fail closed; anything unknown means none):
#   1. Probe: the latest completed run of the hosted-probe workflow succeeded and
#      finished at most PROBE_MAX_AGE seconds ago. A new probe is dispatched whenever the
#      last one is older than PROBE_INTERVAL, so one failed or slow probe still
#      leaves margin before the evidence expires.
#   2. Real CI feedback: among every failed or startup_failed CI run (up to CI_LIST_LIMIT of each) that finished since the good probe, the most recent
#      infrastructure failure on a hosted runner (startup_failure, or a job that
#      failed without ever running a step / getting a runner, which is what
#      exhausted minutes or a spending limit look like) blocks hosted runners
#      until a probe that FINISHED strictly after that failure finished succeeds.
#      Later successful or skipped CI runs never clear the block. Ordinary red
#      builds (tests, clippy) are not infrastructure failures and do not block.
#      If a full page of failures might not reach back past the probe, coverage
#      cannot be proven and hosted stays off (ci=truncated).
# Residual exposure: if minutes run out right after a good probe, runs queued in
# the window fail until either the next probe or the first failed CI run is seen
# (at most about one timer tick for the latter); the window is bounded by
# PROBE_MAX_AGE, it cannot be zero with polling.
#
# Install: run it every 60s from an always-on Linux server with an authenticated
# `gh` (repo admin; scopes repo + workflow), e.g. a systemd timer
# (OnUnitActiveSec=60s) whose service runs `scripts/ci-runner-select.sh`.
# Cost: each probe bills about 1 hosted minute, only while omarchy is offline.
#
# Usage: ci-runner-select.sh [owner/repo]
# Env:   RUNNER_NAME (omarchy-muster), PROBE_WORKFLOW (hosted-probe.yml),
#        CI_WORKFLOW (ci.yml), PROBE_INTERVAL (900), PROBE_MAX_AGE (1800)
# Needs: gh, jq, GNU date.
set -euo pipefail

REPO="${1:-oliveralerubio/muster}"
RUNNER_NAME="${RUNNER_NAME:-omarchy-muster}"
PROBE_WORKFLOW="${PROBE_WORKFLOW:-hosted-probe.yml}"
CI_WORKFLOW="${CI_WORKFLOW:-ci.yml}"
PROBE_INTERVAL="${PROBE_INTERVAL:-900}"
PROBE_MAX_AGE="${PROBE_MAX_AGE:-1800}"
CI_LIST_LIMIT=100  # failures fetched per kind; a full page that may not reach the probe means "truncated"
CI_RUN_MAX_SPAN=21600  # gh lists by creation time; a run can finish up to 6h (job max) after it was created

SELF_HOSTED='["self-hosted","fast-runner"]'
HOSTED='["ubuntu-latest"]'

ts() { date -u -d "$1" +%s 2>/dev/null || echo 0; }

# If runners cannot be queried, omarchy may have disconnected: fail safe to none
# (still published below), then exit non-zero so the timer shows the failure.
exit_code=0
if runners_json="$(gh api "repos/$REPO/actions/runners?per_page=100")"; then
    online="$(jq -r --arg name "$RUNNER_NAME" \
        'any(.runners[]?; .name == $name and .status == "online")' <<<"$runners_json")"
else
    echo "ci-runner-select: cannot query runners for $REPO; failing safe to none" >&2
    online="error"
    exit_code=1
fi

desired="none"
runner="offline"
probe="none"
ci="none"
probed="no"

if [ "$online" = "error" ]; then
    runner="error"
elif [ "$online" = "true" ]; then
    runner="online"
    desired="$SELF_HOSTED"
else
    now="$(date -u +%s)"
    list_failed="no"

    # --- Signal 1: hosted-probe runs (age is measured from when the probe finished)
    if ! probe_runs="$(gh run list --repo "$REPO" --workflow "$PROBE_WORKFLOW" --limit 5 \
        --json status,conclusion,createdAt,updatedAt)"; then
        # Self-hosted is known to be offline: never leave CI pointing at it.
        echo "ci-runner-select: cannot list probe runs for $REPO; failing safe to none" >&2
        probe_runs="[]"
        list_failed="yes"
    fi
    # Latest completed run that is real evidence (cancelled/skipped are not).
    last="$(jq -r '[.[] | select(.status == "completed" and ((.conclusion // "") | IN("cancelled", "skipped") | not))]
        | .[0] // empty | [(.conclusion // ""), .updatedAt] | join("|")' <<<"$probe_runs")"
    pending_created="$(jq -r '[.[] | select(.status != "completed")] | .[0].createdAt // empty' <<<"$probe_runs")"

    age=""
    probe_ts=0
    if [ -n "$last" ]; then
        IFS='|' read -r conclusion finished <<<"$last"
        probe_ts="$(ts "$finished")"
        age=$((now - probe_ts))
        if [ "$conclusion" != "success" ]; then
            probe="failure"
        elif [ "$age" -le "$PROBE_MAX_AGE" ]; then
            probe="success"
        else
            probe="stale"
        fi
    fi
    pending="no"
    if [ -n "$pending_created" ] && [ $((now - $(ts "$pending_created"))) -le "$PROBE_MAX_AGE" ]; then
        pending="yes"
        [ "$probe" = "none" ] && probe="pending"
    fi

    # --- Signal 2: did real CI die on a hosted runner for infrastructure reasons?
    # Query failed runs directly (successful/skipped runs can never push a failure
    # out of the window) and inspect EVERY failure that finished at or after the
    # good probe, newest first (ordinary red builds are skipped over, so they can't
    # hide an infrastructure failure). Failures older than the probe are already
    # superseded by it, so they cost nothing; without a good probe the answer is
    # none anyway and no jobs calls are made. Only a newer probe clears a block.
    since=99999999999
    [ "$probe" = "success" ] && since="$probe_ts"
    ci="ok"
    infra_found="no"
    infra_ts=0
    if ! ci_failed="$(gh run list --repo "$REPO" --workflow "$CI_WORKFLOW" --status failure --limit "$CI_LIST_LIMIT" \
        --json databaseId,conclusion,createdAt,updatedAt)" ||
        ! ci_startup="$(gh run list --repo "$REPO" --workflow "$CI_WORKFLOW" --status startup_failure --limit "$CI_LIST_LIMIT" \
            --json databaseId,conclusion,createdAt,updatedAt)"; then
        echo "ci-runner-select: cannot list $CI_WORKFLOW runs for $REPO; failing safe to none" >&2
        ci="unknown"
    else
        # Prove the lists cover everything since the probe: a full page whose oldest
        # run was created inside the relevant period means older failures may be cut off.
        for list in "$ci_failed" "$ci_startup"; do
            if [ "$(jq -r --argjson since "$since" --argjson lim "$CI_LIST_LIMIT" --argjson span "$CI_RUN_MAX_SPAN" \
                'length >= $lim and ((map(.createdAt | fromdateiso8601) | min) >= ($since - $span))' <<<"$list")" = "true" ]; then
                echo "ci-runner-select: $CI_WORKFLOW failure list may not reach back to the probe; failing safe to none" >&2
                ci="truncated"
                ci_failed="[]"
                ci_startup="[]"
            fi
        done
        while IFS='|' read -r run_id run_conclusion run_updated; do
            [ -n "$run_id" ] || continue
            infra="no"
            if [ "$run_conclusion" = "startup_failure" ]; then
                infra="yes"
            elif ! jobs_json="$(gh api "repos/$REPO/actions/runs/$run_id/jobs?per_page=100")"; then
                echo "ci-runner-select: cannot read jobs of run $run_id; failing safe to none" >&2
                ci="unknown"
                break
            else
                # Hosted job that failed with no steps executed or no runner assigned.
                infra="$(jq -r 'any(.jobs[]?;
                    (.labels // [] | index("ubuntu-latest")) != null
                    and (.conclusion // "") == "failure"
                    and (((.steps // []) | length) == 0 or ((.runner_id // 0) == 0)))' <<<"$jobs_json")"
            fi
            if [ "$infra" = "yes" ] || [ "$infra" = "true" ]; then
                infra_found="yes"
                infra_ts="$(ts "$run_updated")"
                [ "$infra_ts" -gt 0 ] || infra_ts="$now" # unparsable time: block as if it just happened
                break
            fi
        done < <(jq -rs --argjson since "$since" 'add | map(select((.updatedAt | fromdateiso8601) >= $since))
            | sort_by(.updatedAt) | reverse | .[] | [.databaseId, .conclusion, .updatedAt] | join("|")' <<<"$ci_failed $ci_startup")
        # Hosted stays blocked until a probe that finished strictly after the failure succeeded.
        if [ "$ci" = "ok" ] && [ "$infra_found" = "yes" ] && { [ "$probe" != "success" ] || [ "$probe_ts" -le "$infra_ts" ]; }; then
            ci="infra-failure"
        fi
    fi

    if [ "$probe" = "success" ] && [ "$ci" = "ok" ]; then
        desired="$HOSTED"
    fi

    # --- Keep evidence fresh -----------------------------------------------------
    if [ "$list_failed" = "no" ] && [ "$pending" = "no" ] && { [ -z "$age" ] || [ "$age" -gt "$PROBE_INTERVAL" ]; }; then
        if gh workflow run "$PROBE_WORKFLOW" --repo "$REPO" --ref main >/dev/null 2>&1; then
            probed="yes"
        fi
    fi
fi

current="$(gh api "repos/$REPO/actions/variables/CI_RUNNER" --jq .value 2>/dev/null || true)"
changed="no"
if [ "$current" != "$desired" ]; then
    gh variable set CI_RUNNER --repo "$REPO" --body "$desired" >/dev/null
    changed="yes"
fi

echo "CI_RUNNER=$desired runner=$runner probe=$probe ci=$ci changed=$changed probed=$probed"
exit "$exit_code"
