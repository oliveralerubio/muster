---
name: babysitpr
description: "Babysit a pull request from open status to merge: monitor checks, auto-heal or override failing CI/CD, address reviews, and execute merge to production."
---

# Babysit PR (`/babysitpr`)

Automated end-to-end pull request driver that babysits a PR until it is successfully merged into the base branch (`main`/`master`). Handles CI failures, re-runs flaky jobs, applies fixes, resolves conflicts, and completes the merge.

## Triggers

- Slash command: `/babysitpr [PR_NUMBER|PR_URL] [--force-ci] [--strategy squash|rebase|merge]`
- Natural language prompts:
  - "babysit this PR" / "haz babysit al PR"
  - "get this PR green and merged"
  - "encárgate del PR hasta el merge"
  - "mergea si fallan los CI/CD y llévalo a prod"

---

## Operating Phases

### Phase 1: Context & Frontier Resolution

1. Identify target repository and PR:
   - If argument provided: `gh pr view <arg> --json number,title,headRefName,baseRefName,mergeable,state`.
   - If no argument: detect from current branch: `gh pr view --json number,title,headRefName,baseRefName,mergeable,state`.
2. Verify PR state:
   - If merged: exit with confirmation.
   - If closed: report and halt.
3. Ensure base branch is fresh:
   - `git fetch origin <baseRefName>`.

---

### Phase 2: Conflict & Rebase Hygiene

1. Inspect mergeability:
   - `gh pr view --json mergeable`.
2. If `CONFLICTING` or `DIRTY`:
   - Fetch base: `git fetch origin <baseRefName>`.
   - Rebase: `git rebase origin/<baseRefName>`.
   - Resolve conflicts cleanly, preserving feature changes.
   - Run local validation tests.
   - Force push: `git push --force-with-lease origin <headRefName>`.

---

### Phase 3: CI/CD Observation & Healing Loop

1. Poll check runs:
   - Monitor status via `gh pr checks <PR> --watch` (or dynamic self-paced loop).
2. If all required checks pass:
   - Proceed directly to Phase 4 (Review Triage) and Phase 5 (Merge).
3. If checks fail:
   - Inspect failure logs:
     ```bash
     gh run view --log-failed
     ```
   - Classify failure:
     - **Transient / Flake / Network timeout**: Re-run failed jobs via `gh run rerun <run-id> --failed` (max 2 retries).
     - **Code or Test failure in PR diff**: Read stack trace, reproduce locally, apply minimal targeted fix, commit (`fix(ci): <reason>`), and push.
     - **Persistent CI failure / Infrastructure outage / Non-blocking linter**:
       - When code is verified locally and failure is due to external infra, runner availability, or known flaky tests:
         1. Post comment on PR documenting the bypass justification:
            ```bash
            gh pr comment <PR> --body "🤖 CI failed due to external/infra issues. Code verified locally. Proceeding with merge under babysitpr policy."
            ```
         2. Execute merge with `--admin` bypass if branch protection requires it:
            ```bash
            gh pr merge <PR> --admin --squash --delete-branch
            ```

---

### Phase 4: Review Comments & Bot Feedback

1. Inspect review comments and bot feedback:
   ```bash
   gh pr view <PR> --json reviewDecision,comments,reviews
   ```
2. Triage feedback:
   - **Real defect / Security blocker**: Fix in code, commit, push, and cite commit SHA in thread reply.
   - **False positive / Bot hallucination**: Post comment disproving the claim with evidence.
   - **Non-blocking suggestion**: Acknowledge and proceed without stalling the merge.

---

### Phase 5: Merge Execution & Cleanup

1. Merge the PR into base (`main`):
   - Default method is squash merge (or repo convention):
     ```bash
     gh pr merge <PR> --squash --delete-branch
     ```
   - If merge requirements are blocked by CI but local verification passes:
     ```bash
     gh pr merge <PR> --admin --squash --delete-branch
     ```
2. Verify merge on remote:
   ```bash
   gh pr view <PR> --json state,mergedAt,mergeCommit
   ```
3. Local synchronization and cleanup:
   - Switch to base branch: `git checkout <baseRefName>`.
   - Pull latest: `git pull --ff-only origin <baseRefName>`.
   - Delete local branch: `git branch -d <headRefName>`.
   - If running inside a git worktree, remove it: `git worktree remove <path>`.
4. Report final summary:
   - PR URL, title, merge commit SHA.
   - Status of CI and any resolved/bypassed checks.
