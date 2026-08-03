# jj-ryu

<img width="366" height="366" alt="image" src="https://github.com/user-attachments/assets/1691edfc-3b65-4f8d-b959-71ff21ff23e5" />

Stacked PRs for [Jujutsu](https://jj-vcs.github.io/jj/latest/). Push bookmark stacks to GitHub and GitLab as chained pull requests.

## What it does

```
       [feat-c]
    @  mzpwwxkq a1b2c3d4 Add logout       -->   PR #3: feat-c -> feat-b
    |
       [feat-b]
    o  yskvutnz e5f6a7b8 Add sessions     -->   PR #2: feat-b -> feat-a
    |
       [feat-a]
    o  kpqvunts 9d8c7b6a Add auth         -->   PR #1: feat-a -> main
    |
  trunk()
```

Each bookmark becomes a PR targeting the previous bookmark (or trunk). When you update your stack, `ryu` updates the PRs.

## Install

```sh
# npm (prebuilt binaries)
npm install -g jj-ryu

# cargo
cargo install jj-ryu
```

Binary name is `ryu`.

## Quick start

```sh
# View your current stack
ryu

# Track bookmarks for submission
ryu track --all

# Submit tracked bookmarks as PRs
ryu submit

# Sync stack with remote
ryu sync
```

## Authentication

### GitHub

Uses (in order):
1. `gh auth token` (GitHub CLI)
2. `GITHUB_TOKEN` env var
3. `GH_TOKEN` env var

For GitHub Enterprise: `export GH_HOST=github.mycompany.com`

The default `gh` OAuth token scopes are sufficient for the Stacks API and async merge endpoint — no extra authentication setup is needed. On repositories where GitHub stacked PRs aren't available (e.g. some GHES versions), ryu falls back to plain chained PRs: submit succeeds without native stack registration, and `ryu merge`/`ryu unstack` report that the feature is unavailable.

### GitLab

Uses (in order):
1. `glab auth token` (GitLab CLI)
2. `GITLAB_TOKEN` env var
3. `GL_TOKEN` env var

For self-hosted: `export GITLAB_HOST=gitlab.mycompany.com`

### Test authentication

```sh
ryu auth github test
ryu auth gitlab test
```

## Usage

### Viewing your stack

Running `ryu` with no arguments shows the current stack (bookmarks between trunk and working copy):

```
$ ryu

Stack: feat-c

       [feat-c]
    @  yskvutnz e5f6a7b8 Add logout endpoint
    |
       [feat-b] ^
    o  mzwwxrlq a1b2c3d4 Add session management
    |
       [feat-a] *
    o  kpqvunts 7d3a1b2c Add user authentication
    |
  trunk()

3 bookmarks

Legend: * = synced, ^ = needs push, @ = working copy
```

### Tracking bookmarks

Before submitting, bookmarks must be tracked. This gives you control over which bookmarks become PRs:

```sh
# Interactive selection (opens multi-select picker)
ryu track

# Track specific bookmarks
ryu track feat-a feat-b

# Track all bookmarks in trunk()..@
ryu track --all

# Untrack a bookmark
ryu untrack feat-a
```

Tracking state is stored in `.jj/ryu/tracking.json` per workspace.

### Submitting

```sh
ryu submit
```

This pushes all tracked bookmarks in the current stack, creates PRs for any without one, updates PR base branches, and registers them as a native [GitHub stack](https://docs.github.com/en/pull-requests/get-started/about-stacked-prs) (on repositories where stacked PRs are available). Untracked bookmarks are skipped with a warning.

Registered stacks get GitHub's native stack UI: a stack map in the merge box, layer navigation, and server-side cascading rebases after merges.

### Syncing

```sh
ryu sync
```

This fetches from remote and syncs the current stack. On GitHub repos with stacked PRs, `ryu sync` also drops layers whose PRs have merged (deleting the local bookmark and PR cache entry) and rebases the remaining stack onto the updated trunk — GitHub's server-side rebase of the rest of the stack is reconciled automatically on the next push.

### Merging (GitHub Stacks)

```sh
# Merge the bottom unmerged layer of the stack (squash by default)
ryu merge

# Merge through a specific layer (merges it and everything below it)
ryu merge feat-b --method squash
```

Stacked PRs must be merged via GitHub's async merge API: merging a PR merges every unmerged PR below it atomically, and GitHub retargets and rebases the rest of the stack server-side. Afterwards, run `ryu sync` to drop the merged layers locally.

### Unstacking

```sh
ryu unstack
```

Removes all unmerged PRs from the native GitHub stack (the PRs, branches, and local bookmarks are left untouched). Useful before restructuring a stack, since GitHub stacks are additive-only.

## Workflow example

```sh
# Start a feature
jj new main
jj bookmark create feat-auth

# Work on it
jj commit -m "Add user model"

# Stack another change on top
jj bookmark create feat-session
jj commit -m "Add session handling"

# View the stack
ryu

# Track bookmarks for submission
ryu track --all

# Submit both as PRs (feat-session -> feat-auth -> main)
ryu submit

# Make changes, then update PRs
jj commit -m "Address review feedback"
ryu submit

# Merge the bottom PR (and everything below it) once approved
ryu merge feat-auth

# Drop the merged layers and rebase the rest
ryu sync
```

## Advanced options

### Preview and confirmation

```sh
ryu submit feat-c --dry-run    # Preview without making changes
ryu submit feat-c --confirm    # Preview and prompt before executing
```

### Controlling submission scope

```sh
# Submit only up to a specific bookmark (excludes descendants)
ryu submit feat-c --upto feat-b

# Submit only one bookmark (parent must already have a PR)
ryu submit feat-b --only

# Include all descendants in submission
ryu submit feat-a --stack

# Only update existing PRs, don't create new ones
ryu submit feat-c --update-only

# Interactively select which bookmarks to submit
ryu submit feat-c --select
```

### Draft PRs

```sh
# Create new PRs as drafts
ryu submit feat-c --draft

# Publish draft PRs (mark as ready for review)
ryu submit feat-c --publish
```

## CLI reference

```
ryu [OPTIONS] [COMMAND]

Commands:
  submit   Submit tracked bookmarks as PRs
  track    Track bookmarks for submission
  untrack  Stop tracking bookmarks
  sync     Sync all stacks with remote
  merge    Merge a stacked PR and all PRs below it (GitHub Stacks)
  unstack  Remove all unmerged PRs from the native GitHub stack
  auth     Authentication management

Options:
  -p, --path <PATH>  Path to jj repository
  -h, --help         Print help
  -V, --version      Print version
```

### submit

```
ryu submit [OPTIONS]

Options:
      --dry-run          Preview without making changes
  -c, --confirm          Preview and prompt for confirmation
      --upto <BOOKMARK>  Submit only up to this bookmark
      --only <BOOKMARK>  Submit only this bookmark (parent must have PR)
      --update-only      Only update existing PRs
  -s, --stack            Include all descendants in submission
      --draft            Create new PRs as drafts
      --publish          Publish draft PRs
  -i, --select           Interactively select bookmarks
      --remote <REMOTE>  Git remote (default: origin)
```

### track

```
ryu track [BOOKMARKS]... [OPTIONS]

Options:
  -a, --all              Track all bookmarks in trunk()..@
  -f, --force            Re-track already-tracked bookmarks
      --remote <REMOTE>  Associate with specific remote
```

### untrack

```
ryu untrack <BOOKMARKS>...

Options:
  -a, --all              Untrack all bookmarks
```

### sync

```
ryu sync [OPTIONS]

Options:
      --dry-run          Preview without making changes
  -c, --confirm          Preview and prompt for confirmation
      --stack <BOOKMARK> Only sync this stack
      --remote <REMOTE>  Git remote (default: origin)
```

### merge

```
ryu merge [BOOKMARK] [OPTIONS]

Options:
      --method <METHOD>  Merge method: merge, squash (default), or rebase
  -y, --yes              Skip confirmation
      --remote <REMOTE>  Git remote (default: origin)
```

### unstack

```
ryu unstack [OPTIONS]

Options:
  -y, --yes              Skip confirmation
      --remote <REMOTE>  Git remote (default: origin)
```

### auth

```
ryu auth github test    # Test GitHub auth
ryu auth github setup   # Show setup instructions
ryu auth gitlab test    # Test GitLab auth
ryu auth gitlab setup   # Show setup instructions
```

## Coming from Graphite?

Ryu's CLI is inspired by Graphite. Here's how commands map:

| Graphite | Ryu |
|----------|-----|
| `gt track` | `ryu track` |
| `gt submit` | `ryu submit` |
| `gt submit --stack` | `ryu submit --stack` |
| `gt submit --only` | `ryu submit --only <bookmark>` |
| `gt submit --draft` | `ryu submit --draft` |
| `gt submit --publish` | `ryu submit --publish` |
| `gt submit --confirm` | `ryu submit --confirm` |
| `gt sync` | `ryu sync` |
| `gt branch create` | `jj bookmark create` |
| `gt restack` | `jj rebase` |

Key differences:
- Ryu requires explicit tracking before submit (`ryu track`)
- Stack management uses jj commands (`jj bookmark`, `jj rebase`), not ryu
- `ryu sync --stack <bookmark>` syncs a single stack (Graphite syncs all)

## License

MIT
