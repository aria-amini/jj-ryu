//! Tests for sync cleanup of landed bookmarks (`plan_prune` / `apply_prune`)

mod common;

use common::{MockPlatformService, TempJjRepo, github_config, gitlab_config, make_pr};
use jj_ryu::graph::build_change_graph;
use jj_ryu::sync_merged::{
    MergedLayer, PrBookmark, PrunePlan, apply_prune, collect_prune_input, plan_prune,
};
use jj_ryu::tracking::{PrCache, TrackedBookmark, TrackingState};
use jj_ryu::types::{BranchPr, PrState};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// A repo whose trunk is `main@origin`, one commit deep
struct Fixture {
    repo: TempJjRepo,
    _remote: TempDir,
}

fn jj_in(dir: &Path, args: &[&str]) {
    let output = Command::new("jj")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run jj");
    assert!(
        output.status.success(),
        "jj {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn trunk_fixture() -> Fixture {
    let (remote, remote_path) = TempJjRepo::create_bare_remote();
    let repo = TempJjRepo::new();
    repo.add_remote("origin", &remote_path);
    repo.run_jj(&["bookmark", "create", "main", "-r", "@-"]);
    repo.push_bookmark("main", "origin");
    Fixture {
        repo,
        _remote: remote,
    }
}

impl Fixture {
    /// Commit a unique file on top of `base` in `dir`, bookmark it, and leave
    /// the working copy on a new empty child. Returns the bookmarked commit.
    fn branch_in(&self, dir: &Path, name: &str, base: &str) -> String {
        jj_in(dir, &["new", base, "-m", name]);
        std::fs::write(dir.join(format!("{name}.txt")), name).expect("write file");
        jj_in(dir, &["bookmark", "create", name, "-r", "@"]);
        jj_in(dir, &["new"]);
        self.tip(name)
    }

    fn branch(&self, name: &str, base: &str) -> String {
        self.branch_in(self.repo.path(), name, base)
    }

    fn tip(&self, name: &str) -> String {
        self.repo
            .workspace()
            .get_local_bookmark(name)
            .expect("read bookmark")
            .unwrap_or_else(|| panic!("bookmark {name} is missing"))
            .commit_id
    }

    /// Land a squash commit on `main`, leave the working copy empty on it
    fn advance_trunk(&self, label: &str) {
        self.repo.run_jj(&["new", "main", "-m", label]);
        std::fs::write(self.repo.path().join(format!("{label}.txt")), label).expect("write file");
        self.repo.run_jj(&["bookmark", "move", "main", "--to", "@"]);
        self.repo.push_bookmark("main", "origin");
        self.repo.run_jj(&["new", "main"]);
    }

    fn is_visible(&self, commit_id: &str) -> bool {
        !self
            .repo
            .workspace()
            .resolve_revset(&format!("::visible_heads() & {commit_id}"))
            .expect("resolve revset")
            .is_empty()
    }
}

fn pr(number: u64, state: PrState, head_sha: &str) -> BranchPr {
    BranchPr {
        number,
        state,
        head_sha: head_sha.to_string(),
    }
}

async fn plan(f: &Fixture, platform: &MockPlatformService, tracking: &TrackingState) -> PrunePlan {
    let input = collect_prune_input(&f.repo.workspace(), tracking).expect("collect input");
    plan_prune(platform, input).await.expect("plan prune")
}

fn apply(f: &Fixture, plan: &PrunePlan, prune_closed: bool) -> usize {
    let mut workspace = f.repo.workspace();
    let reparented = apply_prune(
        &mut workspace,
        &mut TrackingState::default(),
        &mut PrCache::new(),
        plan,
        prune_closed,
    )
    .expect("apply prune");
    // `ryu sync` follows with this: abandoning can rewrite the working copy
    workspace
        .update_working_copy()
        .expect("update working copy");
    reparented
}

fn bookmarks(f: &Fixture) -> Vec<String> {
    let mut names = f.repo.bookmark_names();
    names.sort();
    names
}

/// A bookmark nobody ran `ryu track` on is cleaned once its PR merged
#[tokio::test]
async fn test_untracked_merged_bookmark_is_deleted() {
    let f = trunk_fixture();
    let tip = f.branch("feat-a", "main");
    f.advance_trunk("Squash merge of feat-a");
    let mock = MockPlatformService::with_config(github_config());
    // GitHub dissolves a native stack once fewer than two PRs remain, so
    // detection must not depend on one.
    mock.stacks_unavailable();
    mock.set_branch_prs("feat-a", vec![pr(7, PrState::Merged, &tip)]);

    let tracking = TrackingState::default();
    let plan = plan(&f, &mock, &tracking).await;
    assert_eq!(
        plan.merged,
        vec![MergedLayer {
            bookmark: "feat-a".into(),
            pr_number: Some(7)
        }]
    );

    assert_eq!(mock.get_find_stack_calls(), Vec::<u64>::new());

    apply(&f, &plan, false);
    assert_eq!(bookmarks(&f), ["main"]);
    // The landed commit is abandoned too, not left as a stray head
    assert!(!f.is_visible(&tip));
}

/// Merged layers of a chained stack are found on GitLab too
#[tokio::test]
async fn test_merged_layers_are_found_on_gitlab() {
    let f = trunk_fixture();
    let tip_1 = f.branch("feat-1", "main");
    let tip_2 = f.branch("feat-2", "feat-1");
    f.branch("feat-3", "feat-2");
    f.advance_trunk("Squash merge of feat-1 and feat-2");
    let mock = MockPlatformService::with_config(gitlab_config());
    mock.set_branch_prs("feat-1", vec![pr(1, PrState::Merged, &tip_1)]);
    mock.set_branch_prs("feat-2", vec![pr(2, PrState::Merged, &tip_2)]);

    let plan = plan(&f, &mock, &TrackingState::default()).await;
    let names: Vec<&str> = plan.merged.iter().map(|m| m.bookmark.as_str()).collect();
    assert_eq!(names, ["feat-1", "feat-2"]);
    apply(&f, &plan, false);
    assert_eq!(bookmarks(&f), ["feat-3", "main"]);
}

/// A bookmark in another workspace's stack is cleaned, and the commits that
/// workspace's working copy sits on are kept
#[tokio::test]
async fn test_bookmark_in_other_workspace_is_deleted_without_rewriting_it() {
    let f = trunk_fixture();
    let other_dir = TempDir::new().expect("temp dir");
    let other = other_dir.path().join("other");
    f.repo
        .run_jj(&["workspace", "add", &other.to_string_lossy()]);
    let tip = f.branch_in(&other, "feat-other", "main");
    f.advance_trunk("Squash merge of feat-other");

    let mock = MockPlatformService::with_config(github_config());
    mock.set_branch_prs("feat-other", vec![pr(3, PrState::Merged, &tip)]);
    let plan = plan(&f, &mock, &TrackingState::default()).await;
    assert_eq!(plan.merged.len(), 1);

    apply(&f, &plan, false);
    assert_eq!(bookmarks(&f), ["main"]);
    // The other workspace's working copy is a child of `tip`: kept as is
    assert!(f.is_visible(&tip));
}

/// Another workspace sitting on the very commit this workspace is on still
/// protects that commit
#[tokio::test]
async fn test_other_workspace_on_same_commit_keeps_it() {
    let f = trunk_fixture();
    let tip = f.branch("feat-shared", "main");
    f.advance_trunk("Squash merge of feat-shared");
    let other_dir = TempDir::new().expect("temp dir");
    let other = other_dir.path().join("other");
    f.repo
        .run_jj(&["workspace", "add", &other.to_string_lossy()]);
    // Both workspaces now have the bookmarked commit as their working copy
    jj_in(&other, &["edit", &tip]);
    f.repo.run_jj(&["edit", &tip]);

    let mock = MockPlatformService::with_config(github_config());
    mock.set_branch_prs("feat-shared", vec![pr(3, PrState::Merged, &tip)]);
    let plan = plan(&f, &mock, &TrackingState::default()).await;
    apply(&f, &plan, false);

    assert_eq!(bookmarks(&f), ["main"]);
    assert!(f.is_visible(&tip));
}

/// A dropped layer in the current stack that another workspace also builds on
/// is neither abandoned nor rebased: both workspaces keep their history
#[tokio::test]
async fn test_layer_shared_with_other_workspace_is_not_rewritten() {
    let f = trunk_fixture();
    let tip = f.branch("feat-shared", "main");
    f.advance_trunk("Squash merge of feat-shared");
    let other_dir = TempDir::new().expect("temp dir");
    let other = other_dir.path().join("other");
    f.repo
        .run_jj(&["workspace", "add", &other.to_string_lossy()]);
    jj_in(&other, &["new", &tip]);
    f.repo.run_jj(&["new", &tip]);

    let mock = MockPlatformService::with_config(github_config());
    mock.set_branch_prs("feat-shared", vec![pr(3, PrState::Merged, &tip)]);
    let plan = plan(&f, &mock, &TrackingState::default()).await;
    assert_eq!(apply(&f, &plan, false), 0);

    assert_eq!(bookmarks(&f), ["main"]);
    assert!(f.is_visible(&tip));
}

/// One kept layer is enough to skip the rebase, even when another dropped
/// layer of the same stack was fully abandoned
#[tokio::test]
async fn test_kept_layer_blocks_rebase_for_other_dropped_layers() {
    let f = trunk_fixture();
    let tip_a = f.branch("feat-a", "main");
    let tip_b = f.branch("feat-b", "main");
    f.advance_trunk("Squash merge of feat-a and feat-b");
    let other_dir = TempDir::new().expect("temp dir");
    let other = other_dir.path().join("other");
    f.repo
        .run_jj(&["workspace", "add", &other.to_string_lossy()]);
    jj_in(&other, &["new", &tip_a]);
    // The current stack merges both layers
    f.repo.run_jj(&["new", &tip_a, &tip_b]);

    let mock = MockPlatformService::with_config(github_config());
    mock.set_branch_prs("feat-a", vec![pr(1, PrState::Merged, &tip_a)]);
    mock.set_branch_prs("feat-b", vec![pr(2, PrState::Merged, &tip_b)]);
    let plan = plan(&f, &mock, &TrackingState::default()).await;
    assert_eq!(plan.merged.len(), 2);
    assert_eq!(apply(&f, &plan, false), 0);

    assert_eq!(bookmarks(&f), ["main"]);
    assert!(f.is_visible(&tip_a));
    assert!(!f.is_visible(&tip_b));
}

/// With an empty working copy at trunk there is no stack, but cleanup works
#[tokio::test]
async fn test_cleanup_works_with_empty_working_copy_at_trunk() {
    let f = trunk_fixture();
    let tip = f.branch("feat-a", "main");
    f.advance_trunk("Squash merge of feat-a");
    assert!(
        build_change_graph(&f.repo.workspace())
            .expect("graph")
            .stack
            .is_none()
    );

    let mock = MockPlatformService::with_config(github_config());
    mock.set_branch_prs("feat-a", vec![pr(1, PrState::Merged, &tip)]);
    let plan = plan(&f, &mock, &TrackingState::default()).await;
    assert_eq!(apply(&f, &plan, false), 0);
    assert_eq!(bookmarks(&f), ["main"]);
}

/// A fresh task bookmark on the trunk tip has nothing yet: it stays unless a
/// merged PR from that exact commit says otherwise. `main` is never a candidate.
#[tokio::test]
async fn test_trunk_tip_bookmark_needs_pr_evidence() {
    let f = trunk_fixture();
    f.repo
        .run_jj(&["bookmark", "create", "fresh", "-r", "main"]);
    let tip = f.tip("main");
    let mock = MockPlatformService::with_config(github_config());
    // `main` itself has a "merged PR" at the tip; it must still be ignored
    mock.set_branch_prs("main", vec![pr(1, PrState::Merged, &tip)]);

    let tracking = TrackingState::default();
    let plan_without_pr = plan(&f, &mock, &tracking).await;
    assert_eq!(plan_without_pr, PrunePlan::default());
    assert!(!mock.get_branch_pr_calls().contains(&"main".to_string()));

    // A fast-forward merge keeps the PR head at the trunk tip: now it is landed
    mock.set_branch_prs("fresh", vec![pr(2, PrState::Merged, &tip)]);
    let plan_with_pr = plan(&f, &mock, &tracking).await;
    assert_eq!(
        plan_with_pr.merged,
        vec![MergedLayer {
            bookmark: "fresh".into(),
            pr_number: Some(2)
        }]
    );
    apply(&f, &plan_with_pr, false);
    assert_eq!(bookmarks(&f), ["main"]);
}

/// A bookmark strictly behind trunk has nothing unique: deleted without PR
/// evidence and without asking the platform
#[tokio::test]
async fn test_ancestor_of_trunk_is_deleted_without_api_call() {
    let f = trunk_fixture();
    f.repo.run_jj(&["bookmark", "create", "old", "-r", "main"]);
    f.advance_trunk("Later work");
    let mock = MockPlatformService::with_config(github_config());

    let plan = plan(&f, &mock, &TrackingState::default()).await;
    assert_eq!(
        plan.merged,
        vec![MergedLayer {
            bookmark: "old".into(),
            pr_number: None
        }]
    );
    assert_eq!(mock.get_branch_pr_calls(), Vec::<String>::new());
}

/// Closed without merging: reported, kept, and deleted only with the flag.
/// Its commits stay visible either way.
#[tokio::test]
async fn test_closed_pr_is_reported_and_pruned_only_with_flag() {
    let f = trunk_fixture();
    let tip = f.branch("feat-closed", "main");
    let mock = MockPlatformService::with_config(github_config());
    mock.set_branch_prs("feat-closed", vec![pr(5, PrState::Closed, &tip)]);

    let plan = plan(&f, &mock, &TrackingState::default()).await;
    assert_eq!(plan.merged, []);
    assert_eq!(
        plan.closed,
        vec![PrBookmark {
            bookmark: "feat-closed".into(),
            pr_number: 5
        }]
    );

    apply(&f, &plan, false);
    assert_eq!(bookmarks(&f), ["feat-closed", "main"]);

    apply(&f, &plan, true);
    assert_eq!(bookmarks(&f), ["main"]);
    assert!(f.is_visible(&tip));
}

/// Planning reads only: a dry run (plan without apply) changes nothing
#[tokio::test]
async fn test_plan_changes_nothing() {
    let f = trunk_fixture();
    let tip = f.branch("feat-a", "main");
    f.advance_trunk("Squash merge of feat-a");
    let mock = MockPlatformService::with_config(github_config());
    mock.set_branch_prs("feat-a", vec![pr(1, PrState::Merged, &tip)]);
    let mut tracking = TrackingState::default();
    tracking.track(TrackedBookmark::new("feat-a".into(), "change".into()));
    let ops_before = f
        .repo
        .run_jj(&["op", "log", "--no-graph", "-T", "id.short()"]);

    let plan = plan(&f, &mock, &tracking).await;
    assert_eq!(plan.merged.len(), 1);

    assert_eq!(bookmarks(&f), ["feat-a", "main"]);
    assert!(tracking.is_tracked("feat-a"));
    assert!(f.is_visible(&tip));
    let ops_after = f
        .repo
        .run_jj(&["op", "log", "--no-graph", "-T", "id.short()"]);
    assert_eq!(ops_before, ops_after);
}

/// No PR and not in trunk, or an open PR: never touched
#[tokio::test]
async fn test_unmerged_and_open_bookmarks_are_untouched() {
    let f = trunk_fixture();
    f.branch("no-pr", "main");
    let open_tip = f.branch("open-pr", "main");
    let mock = MockPlatformService::with_config(github_config());
    mock.set_branch_prs(
        "open-pr",
        vec![
            pr(9, PrState::Open, &open_tip),
            pr(4, PrState::Merged, &open_tip),
        ],
    );

    let plan = plan(&f, &mock, &TrackingState::default()).await;
    assert_eq!(plan, PrunePlan::default());
    apply(&f, &plan, true);
    assert_eq!(bookmarks(&f), ["main", "no-pr", "open-pr"]);
}

/// A merged PR for a reused branch name must not delete new local work
#[tokio::test]
async fn test_merged_pr_with_different_head_is_kept_and_reported() {
    let f = trunk_fixture();
    f.branch("reused", "main");
    let mock = MockPlatformService::with_config(github_config());
    mock.set_branch_prs("reused", vec![pr(2, PrState::Merged, "0123456789abcdef")]);

    let plan = plan(&f, &mock, &TrackingState::default()).await;
    assert_eq!(plan.merged, []);
    assert_eq!(
        plan.diverged,
        vec![PrBookmark {
            bookmark: "reused".into(),
            pr_number: 2
        }]
    );
    apply(&f, &plan, true);
    assert_eq!(bookmarks(&f), ["main", "reused"]);
}

/// Tracking and cache entries go with the bookmark; entries for bookmarks
/// that no longer exist are pruned
#[tokio::test]
async fn test_tracking_and_cache_are_pruned() {
    let f = trunk_fixture();
    let tip = f.branch("feat-a", "main");
    f.advance_trunk("Squash merge of feat-a");
    let mock = MockPlatformService::with_config(github_config());
    mock.set_branch_prs("feat-a", vec![pr(1, PrState::Merged, &tip)]);
    let mut tracking = TrackingState::default();
    for name in ["feat-a", "gone"] {
        tracking.track(TrackedBookmark::new(name.into(), "change".into()));
    }
    let mut pr_cache = PrCache::new();
    pr_cache.upsert("feat-a", &make_pr(1, "feat-a", "main"), "origin");
    pr_cache.upsert("gone", &make_pr(2, "gone", "main"), "origin");

    let plan = plan(&f, &mock, &tracking).await;
    assert_eq!(plan.dangling, ["gone"]);
    let mut workspace = f.repo.workspace();
    apply_prune(&mut workspace, &mut tracking, &mut pr_cache, &plan, false).expect("apply");

    assert_eq!(tracking.tracked_names(), Vec::<&str>::new());
    assert!(pr_cache.get("feat-a").is_none());
    assert!(pr_cache.get("gone").is_none());
}

/// Cleaning an unrelated bookmark must not rebase the current stack
#[tokio::test]
async fn test_unrelated_cleanup_does_not_rebase_current_stack() {
    let f = trunk_fixture();
    f.repo.run_jj(&["bookmark", "create", "old", "-r", "main"]);
    let stack_tip = f.branch("feat-stack", "main");
    f.advance_trunk("Later work");
    f.repo.run_jj(&["new", &stack_tip]);
    let mock = MockPlatformService::with_config(github_config());

    let plan = plan(&f, &mock, &TrackingState::default()).await;
    assert_eq!(plan.merged.len(), 1);
    assert_eq!(apply(&f, &plan, false), 0);

    assert_eq!(bookmarks(&f), ["feat-stack", "main"]);
    assert_eq!(f.tip("feat-stack"), stack_tip);
}
