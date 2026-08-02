# RFC: Native GitHub Stacks Integration

**Status:** Approved (in progress)
**Date:** 2026-08-02
**Scope:** Replace stack navigation comments with native GitHub Stacks registration;
add `merge`/`unstack` commands; teach `sync` about server-side rebases.

---

## Background

GitHub shipped stacked pull requests in public preview (2026-07-30 changelog,
REST API version `2026-03-10`): a Stacks REST API, read-only GraphQL stack fields,
native stack UI (stack map, layer navigation), server-side cascading rebases after
merges, and a dedicated async merge endpoint. ryu's stack navigation comments
(previously the only stack UI) are removed in favor of the native UI.

## API summary (verified against docs.github.com)

- `GET /repos/{o}/{r}/stacks?pull_request=N` — stacks containing a PR. **404 = feature
  unavailable for the repo** (rollout/GHES).
- `POST /repos/{o}/{r}/stacks` — create; body `{"pull_requests": [n...]}` bottom→top,
  min 2, max 100. Each PR's base ref must equal the previous PR's head ref (422 otherwise).
  Not idempotent; clients must pre-check.
- `POST /repos/{o}/{r}/stacks/{n}/add` — append to top; 409 = concurrent modification
  (retry with backoff); 404 = stack dissolved (fall back to create).
- `POST /repos/{o}/{r}/stacks/{n}/unstack` — removes all unmerged PRs (200 updated /
  204 dissolved). No single-PR removal; restructure = unstack + recreate.
- `PUT /repos/{o}/{r}/pulls/{n}/merge-async` — **required** for merging stacked PRs.
  Merging PR #k merges all PRs below it atomically; the rest are retargeted and
  server-side cascading-rebased (force-pushed). 202 + uuid; poll
  `GET /pulls/{n}/merge-async/{uuid}` → `pending|merged|enqueued|failed`.
- Constraints: same-repo only (matches ryu's existing same-repo PR creation), linear
  chains, no auto-merge, bottom-up merges only.

## Design

### Client (`src/platform/github/stacks.rs`)

Raw `reqwest`, not octocrab: these endpoints are status-code sensitive (404 means both
"feature unavailable" and "stack gone" depending on route, 409 requires retry,
merge-async distinguishes 200/202). Dedicated `stacks_http` client pins
`X-GitHub-Api-Version: 2026-03-10`. Reuses `repo_route()` so GHES base URLs work.

### Trait (`src/platform/mod.rs`)

Seven methods with default `Err(Error::Unsupported)`: `supports_native_stacks`,
`find_stack_for_pr`, `create_stack`, `add_to_stack`, `unstack`, `merge_pr_async`,
`poll_merge_async`. GitHub overrides all; GitLab inherits defaults.

### Submit registration (`src/submit/stack_register.rs`)

Replaces the old comment phase. Entirely soft-fail: registration problems never fail a
submission whose PRs were pushed/created. Upsert algorithm (mirrors `gh stack link`):

1. `find_stack_for_pr` for each submitted PR; PRs spanning two stacks → conflict.
2. No stack → `Create` (after validating base/head continuity).
3. Remote open PRs == ours → `NoOp`. Ours is a bottom slice of a taller remote stack
   (e.g. `--upto`) → `NoOp`.
4. Remote is an ordered prefix of ours → `Add` with the delta.
5. Anything else (reorder/removal) → `ReorderConflict`, advising `ryu unstack`.

409 on `Add` retries with 250/500/1000ms backoff; `StackNotFound` on `Add` falls back
to `Create` (stack dissolved between check and write). Fewer than 2 PRs → `Skip`.
Dry-run reports the intended action (bookmark names when PRs don't exist yet).

### Availability policy

- `Error::Unsupported` (GitLab): silent skip in submit/view; fatal only for explicit
  `merge`/`unstack`.
- `Error::StacksUnavailable` (404/403 at runtime — GHES or rollout-off): submit
  succeeds with a soft warning; view omits stack info; sync skips merged-layer
  detection; `merge`/`unstack` fail with an actionable message.

## Uncertainties to verify empirically

1. Whether merge-async 409 returns the in-flight uuid.
2. `GET /stacks?pull_request=N` on an enabled repo with an unstacked PR → empty array.
3. Fine-grained PAT permissions for stacks/merge-async (only `gh` OAuth is proven).
4. Whether merge-async accepts `sha` for stacked PRs (drop the field if 422).
5. GHES version floor for the endpoints (affects 403-vs-404 mapping).
