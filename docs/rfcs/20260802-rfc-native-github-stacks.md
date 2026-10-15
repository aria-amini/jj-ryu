# RFC: Native GitHub Stacks Integration

**Status:** Implemented
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

### `ryu unstack` (`src/unstack.rs`, `src/cli/unstack.rs`)

Finds the native stack containing any submitted PR (via the PR cache), shows a
summary, confirms, and calls the unstack endpoint — removing all unmerged PRs from
the GitHub-side grouping. Local bookmarks, branches, tracking state, and the PRs
themselves are untouched. This is the supported escape hatch before restructuring,
since the Stacks API is additive-only (no single-PR removal, no reordering).

### `ryu` view (`src/cli/analyze.rs`)

When the default remote is GitHub and the bottom submitted PR belongs to a native
stack, each bookmark line is annotated with its stack position (`stack #7 2/5`) or
`merged`. One `find_stack_for_pr` call per view; every failure mode (no auth, GHES,
GitLab, unstacked) silently renders the pre-stacks view.

### `ryu merge` (`src/merge.rs`, `src/cli/merge.rs`)

Merging a stacked PR merges every unmerged PR below it, so the command takes an
optional bookmark (default: the bottom unmerged layer) and always merges bottom-up:

1. Resolve the target PR (cache → `find_existing_pr`), find its stack.
2. `plan_merge` computes the affected layers (unmerged entries through the target);
   already-merged targets and non-stacked PRs are actionable errors.
3. Confirm, then `merge_pr_async` with the entry's head SHA. `AlreadyMergedOrQueued`
   is informational; `MergeInProgress` (409) tells the user to wait.
4. `poll_merge` polls the UUID every 2s (120s timeout); interval/timeout are
   parameters for tests. Timeout prints the UUID for manual checking.
5. On success, merged layers are removed from the PR cache; the user is told to run
   `ryu sync`.

### `ryu sync` merged layers (`src/sync_merged.rs`)

After fetch, `detect_merged_layers` matches tracked bookmarks against stack entries
with `merged_at` set. For each merged layer, `apply_merged_layers` deletes the local
bookmark, abandons the merged commit (children reparent onto its parents), untracks
it, and removes its cache entry; finally `rebase_stack_onto_trunk` reparents the
stack roots onto the new trunk tip and rebases descendants.

**Deliberate tradeoff:** the server-rewritten remote branches are *not* adopted
directly. After the local rebase, the normal submit machinery force-pushes (with
lease) locally-rebased branches whose content is equivalent to GitHub's server-side
rebase. This is uniform for all cases — clean rebases, local-unique commits, and
divergence all take the same path — at the cost of one extra head-sha rewrite on
each remaining PR. Adopting remote tips when the local bookmark has no unique
commits is a possible future optimization.

## Uncertainties to verify empirically

1. Whether merge-async 409 returns the in-flight uuid.
2. `GET /stacks?pull_request=N` on an enabled repo with an unstacked PR → empty array.
3. Fine-grained PAT permissions for stacks/merge-async (only `gh` OAuth is proven).
4. Whether merge-async accepts `sha` for stacked PRs (drop the field if 422).
5. GHES version floor for the endpoints (affects 403-vs-404 mapping).
