//! Integration tests for jj-ryu

#![allow(deprecated)] // cargo_bin is the standard way to test CLI binaries

mod common;

use assert_cmd::Command;
use common::{MockPlatformService, TempJjRepo, github_config, make_pr};
use jj_ryu::graph::build_change_graph;
use jj_ryu::submit::{ExecutionStep, analyze_submission, create_submission_plan};
use predicates::prelude::*;

#[derive(Default)]
struct MessageProgress(std::sync::Mutex<Vec<String>>);

#[async_trait::async_trait]
impl jj_ryu::submit::ProgressCallback for MessageProgress {
    async fn on_phase(&self, _: jj_ryu::submit::Phase) {}
    async fn on_bookmark_push(&self, _: &str, _: jj_ryu::submit::PushStatus) {}
    async fn on_pr_created(&self, _: &str, _: &jj_ryu::types::PullRequest) {}
    async fn on_pr_updated(&self, _: &str, _: &jj_ryu::types::PullRequest) {}
    async fn on_error(&self, _: &jj_ryu::error::Error) {}
    async fn on_message(&self, message: &str) {
        self.0.lock().unwrap().push(message.to_string());
    }
}

#[tokio::test]
async fn test_dry_run_projects_base_updates_and_stack_recreation() {
    use common::mock_platform::make_stack;
    use jj_ryu::submit::execute_submission;

    let repo = TempJjRepo::new();
    repo.build_stack(&[("feat-a", "Add A"), ("feat-b", "Add B")]);
    let mut workspace = repo.workspace();
    let graph = build_change_graph(&workspace).unwrap();
    let analysis = analyze_submission(&graph, Some("feat-b")).unwrap();

    // Cover both a broken chain and a stack dissolved before retargeting.
    for existing_stack in [false, true] {
        let mock = MockPlatformService::with_config(github_config());
        mock.set_find_pr_response("feat-a", Some(make_pr(1, "feat-a", "main")));
        mock.set_find_pr_response("feat-b", Some(make_pr(2, "feat-b", "main")));
        let plan = create_submission_plan(&analysis, &mock, "origin", "main")
            .await
            .unwrap();
        if existing_stack {
            for number in [1, 2] {
                mock.set_stack_for_pr(number, Some(make_stack(7, &[1, 2])));
            }
        }
        let progress = MessageProgress::default();
        execute_submission(&plan, &mut workspace, &mock, &progress, true)
            .await
            .unwrap();
        assert_eq!(
            progress.0.lock().unwrap().last().map(String::as_str),
            Some("  → create native stack: #1 → #2")
        );
        assert_eq!(plan.existing_prs["feat-b"].base_ref, "main");
        assert_eq!(mock.get_create_stack_calls(), Vec::<Vec<u64>>::new());
        assert_eq!(mock.get_unstack_calls(), Vec::<u64>::new());
    }
}

// =============================================================================
// CLI Tests
// =============================================================================

#[test]
fn test_cli_help() {
    let mut cmd = Command::cargo_bin("ryu").unwrap();
    cmd.arg("--help");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Stacked PRs for Jujutsu"));
}

#[test]
fn test_cli_version() {
    let mut cmd = Command::cargo_bin("ryu").unwrap();
    cmd.arg("--version");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_submit_help() {
    let mut cmd = Command::cargo_bin("ryu").unwrap();
    cmd.args(["submit", "--help"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Submit current stack"));
}

#[test]
fn test_sync_help() {
    let mut cmd = Command::cargo_bin("ryu").unwrap();
    cmd.args(["sync", "--help"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Sync current stack"));
}

#[test]
fn test_auth_help() {
    let mut cmd = Command::cargo_bin("ryu").unwrap();
    cmd.args(["auth", "--help"]);

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("github"))
        .stdout(predicate::str::contains("gitlab"));
}

#[test]
fn test_invalid_path() {
    let mut cmd = Command::cargo_bin("ryu").unwrap();
    cmd.args(["--path", "/nonexistent/path/to/repo"]);

    cmd.assert().failure();
}

// =============================================================================
// Submit Flow Tests
// =============================================================================

#[test]
fn test_temp_repo_graph_building() {
    let repo = TempJjRepo::new();
    repo.build_stack(&[("feat-a", "Add A"), ("feat-b", "Add B")]);

    let workspace = repo.workspace();
    let graph = build_change_graph(&workspace).expect("build graph");

    // Should have both bookmarks
    assert!(graph.bookmarks.contains_key("feat-a"));
    assert!(graph.bookmarks.contains_key("feat-b"));

    // Should have one stack with two segments
    let stack = graph.stack.as_ref().expect("test expects stack");
    assert_eq!(stack.segments.len(), 2);
}

#[test]
fn test_non_linear_stack_reports_unsupported_merge() {
    let repo = TempJjRepo::new();
    for (bookmark, message) in [
        ("bookmark-b", "feature B"),
        ("bookmark-c", "feature C"),
        ("bookmark-d", "feature D"),
    ] {
        repo.run_jj(&["new", "root()", "-m", message]);
        repo.create_bookmark(bookmark);
    }
    repo.run_jj(&[
        "new",
        "bookmark-b",
        "bookmark-c",
        "bookmark-d",
        "-m",
        "merge",
    ]);

    let mut cmd = Command::cargo_bin("ryu").unwrap();
    cmd.current_dir(repo.path());

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains(
            "unsupported non-linear stack: merge commit",
        ))
        .stderr(predicate::str::contains("rebase into a linear stack"))
        .stderr(predicate::str::contains("No bookmark stack found").not());
}

#[test]
fn test_analyze_real_repo_stack() {
    let repo = TempJjRepo::new();
    repo.build_stack(&[
        ("feat-a", "Add A"),
        ("feat-b", "Add B"),
        ("feat-c", "Add C"),
    ]);

    let workspace = repo.workspace();
    let graph = build_change_graph(&workspace).expect("build graph");

    // Analyze middle of stack
    let analysis = analyze_submission(&graph, Some("feat-b")).expect("analyze");

    assert_eq!(analysis.target_bookmark, "feat-b");
    assert_eq!(analysis.segments.len(), 2);
    assert_eq!(analysis.segments[0].bookmark.name, "feat-a");
    assert_eq!(analysis.segments[1].bookmark.name, "feat-b");
}

#[tokio::test]
async fn test_full_submit_flow_new_stack() {
    let repo = TempJjRepo::new();
    repo.build_stack(&[("feat-a", "Add feature A"), ("feat-b", "Add feature B")]);

    let workspace = repo.workspace();
    let graph = build_change_graph(&workspace).expect("build graph");
    let analysis = analyze_submission(&graph, Some("feat-b")).expect("analyze");

    // Mock returns None for all find_existing_pr calls (default behavior)
    let mock = MockPlatformService::with_config(github_config());

    let plan = create_submission_plan(&analysis, &mock, "origin", "main")
        .await
        .expect("create plan");

    // Verify plan
    assert_eq!(plan.count_creates(), 2);
    assert_eq!(plan.count_pushes(), 2);
    assert_eq!(plan.count_updates(), 0);

    // Find CreatePr steps and verify base branches
    let creates: Vec<_> = plan
        .execution_steps
        .iter()
        .filter_map(|s| match s {
            ExecutionStep::CreatePr(c) => Some(c),
            _ => None,
        })
        .collect();

    assert_eq!(creates[0].base_branch, "main");
    assert_eq!(creates[1].base_branch, "feat-a");

    // Verify titles are not empty
    assert_ne!(creates[0].title, "");
    assert_ne!(creates[1].title, "");
}

#[tokio::test]
async fn test_submit_flow_partial_existing_prs() {
    let repo = TempJjRepo::new();
    repo.build_stack(&[("feat-a", "Add A"), ("feat-b", "Add B")]);

    let workspace = repo.workspace();
    let graph = build_change_graph(&workspace).expect("build graph");
    let analysis = analyze_submission(&graph, Some("feat-b")).expect("analyze");

    let mock = MockPlatformService::with_config(github_config());
    // First PR exists
    mock.set_find_pr_response("feat-a", Some(make_pr(1, "feat-a", "main")));
    // Second PR doesn't exist (default)

    let plan = create_submission_plan(&analysis, &mock, "origin", "main")
        .await
        .expect("create plan");

    // Only one PR to create
    assert_eq!(plan.count_creates(), 1);

    let create = plan
        .execution_steps
        .iter()
        .find_map(|s| match s {
            ExecutionStep::CreatePr(c) => Some(c),
            _ => None,
        })
        .expect("should have create step");

    assert_eq!(create.bookmark.name, "feat-b");

    // One existing PR
    assert_eq!(plan.existing_prs.len(), 1);
    assert!(plan.existing_prs.contains_key("feat-a"));
}

#[tokio::test]
async fn test_submit_flow_base_update_needed() {
    let repo = TempJjRepo::new();
    repo.build_stack(&[("feat-a", "Add A"), ("feat-b", "Add B")]);

    let workspace = repo.workspace();
    let graph = build_change_graph(&workspace).expect("build graph");
    let analysis = analyze_submission(&graph, Some("feat-b")).expect("analyze");

    let mock = MockPlatformService::with_config(github_config());
    // Both PRs exist
    mock.set_find_pr_response("feat-a", Some(make_pr(1, "feat-a", "main")));
    // Second PR has wrong base (should be feat-a, is main)
    mock.set_find_pr_response("feat-b", Some(make_pr(2, "feat-b", "main")));

    let plan = create_submission_plan(&analysis, &mock, "origin", "main")
        .await
        .expect("create plan");

    // No PRs to create
    assert_eq!(plan.count_creates(), 0);

    // One PR needs base update
    assert_eq!(plan.count_updates(), 1);

    let update = plan
        .execution_steps
        .iter()
        .find_map(|s| match s {
            ExecutionStep::UpdateBase(u) => Some(u),
            _ => None,
        })
        .expect("should have update step");

    assert_eq!(update.bookmark.name, "feat-b");
    assert_eq!(update.current_base, "main");
    assert_eq!(update.expected_base, "feat-a");
}

#[test]
fn test_single_bookmark_stack() {
    let repo = TempJjRepo::new();
    repo.build_stack(&[("feat-a", "Add feature A")]);

    let workspace = repo.workspace();
    let graph = build_change_graph(&workspace).expect("build graph");
    let analysis = analyze_submission(&graph, Some("feat-a")).expect("analyze");

    assert_eq!(analysis.segments.len(), 1);
    assert_eq!(analysis.segments[0].bookmark.name, "feat-a");
}

// =============================================================================
// Edge Case Tests
// =============================================================================

#[test]
fn test_empty_repo_no_bookmarks() {
    let repo = TempJjRepo::new();
    // Don't create any bookmarks, just use initial state

    let workspace = repo.workspace();
    let graph = build_change_graph(&workspace).expect("build graph");

    // Should have no stacks
    assert!(graph.stack.is_none());
    assert!(graph.bookmarks.is_empty());
}

#[test]
fn test_three_level_deep_stack() {
    let repo = TempJjRepo::new();
    repo.build_stack(&[
        ("feat-a", "Add feature A"),
        ("feat-b", "Add feature B"),
        ("feat-c", "Add feature C"),
    ]);

    let workspace = repo.workspace();
    let graph = build_change_graph(&workspace).expect("build graph");

    let stack = graph.stack.as_ref().expect("test expects stack");
    assert_eq!(stack.segments.len(), 3);

    // Verify ordering: root to leaf
    assert_eq!(stack.segments[0].bookmarks[0].name, "feat-a");
    assert_eq!(stack.segments[1].bookmarks[0].name, "feat-b");
    assert_eq!(stack.segments[2].bookmarks[0].name, "feat-c");
}

#[tokio::test]
async fn test_plan_verifies_pr_queries_for_stack() {
    let repo = TempJjRepo::new();
    repo.build_stack(&[
        ("feat-a", "Add A"),
        ("feat-b", "Add B"),
        ("feat-c", "Add C"),
    ]);

    let workspace = repo.workspace();
    let graph = build_change_graph(&workspace).expect("build graph");
    let analysis = analyze_submission(&graph, Some("feat-c")).expect("analyze");

    let mock = MockPlatformService::with_config(github_config());

    let _ = create_submission_plan(&analysis, &mock, "origin", "main")
        .await
        .expect("create plan");

    // Verify that platform was queried for each bookmark in the stack
    mock.assert_find_pr_called_for(&["feat-a", "feat-b", "feat-c"]);
}

#[tokio::test]
async fn test_plan_pr_numbers_increment() {
    let repo = TempJjRepo::new();
    repo.build_stack(&[("feat-a", "Add A"), ("feat-b", "Add B")]);

    let workspace = repo.workspace();
    let graph = build_change_graph(&workspace).expect("build graph");
    let analysis = analyze_submission(&graph, Some("feat-b")).expect("analyze");

    let mock = MockPlatformService::with_config(github_config());

    let plan = create_submission_plan(&analysis, &mock, "origin", "main")
        .await
        .expect("create plan");

    // Verify we have 2 PRs to create
    assert_eq!(plan.count_creates(), 2);

    // Note: PR creation happens during execute, not planning
    // This test verifies the plan structure is correct
    let creates: Vec<_> = plan
        .execution_steps
        .iter()
        .filter_map(|s| match s {
            ExecutionStep::CreatePr(c) => Some(c),
            _ => None,
        })
        .collect();

    assert_eq!(creates[0].bookmark.name, "feat-a");
    assert_eq!(creates[1].bookmark.name, "feat-b");
}

// =============================================================================
// Git Integration Tests
// =============================================================================

fn bare_remote_bookmark_oid(remote_path: &std::path::Path, bookmark: &str) -> String {
    let output = StdCommand::new("git")
        .args(["rev-parse", &format!("refs/heads/{bookmark}")])
        .current_dir(remote_path)
        .output()
        .expect("read bare remote bookmark");
    assert!(
        output.status.success(),
        "remote bookmark should exist: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn remote_bookmark_state(
    repo_path: &std::path::Path,
    bookmark: &str,
    remote: &str,
) -> jj_lib::op_store::RemoteRefState {
    use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::ref_name::{RefName, RemoteName};
    use jj_lib::settings::UserSettings;
    use jj_lib::workspace::Workspace;

    let mut config = StackedConfig::with_defaults();
    let mut user_layer = ConfigLayer::empty(ConfigSource::User);
    user_layer
        .set_value("user.name", "jj-ryu-test")
        .expect("set test user name");
    user_layer
        .set_value("user.email", "jj-ryu-test@localhost")
        .expect("set test user email");
    config.add_layer(user_layer);
    let settings = UserSettings::from_config(config).expect("create test settings");
    let workspace = Workspace::load(
        &settings,
        repo_path,
        &default_backend_factories(),
        &default_working_copy_factories(),
    )
    .expect("load jj workspace");
    let repo = pollster::block_on(workspace.repo_loader().load_at_head()).expect("load jj repo");
    let symbol = RefName::new(bookmark).to_remote_symbol(RemoteName::new(remote));
    repo.view().get_remote_bookmark(symbol).state
}

#[test]
fn test_git_push_updates_remote_and_tracking_bookmark() {
    let repo = TempJjRepo::new();
    repo.commit("Add feature A");
    repo.create_bookmark("feat-a");
    let (_remote_dir, remote_path) = TempJjRepo::create_bare_remote();
    repo.add_remote("origin", &remote_path);

    let mut workspace = repo.workspace();
    workspace
        .git_push("feat-a", "origin")
        .expect("push feat-a bookmark");

    let remote_oid = bare_remote_bookmark_oid(&remote_path, "feat-a");
    let bookmark = workspace
        .get_local_bookmark("feat-a")
        .expect("read local bookmark")
        .expect("feat-a bookmark should exist");
    assert_eq!(remote_oid, bookmark.commit_id);
    assert_eq!(
        remote_bookmark_state(repo.path(), "feat-a", "origin"),
        jj_lib::op_store::RemoteRefState::Tracked
    );
    assert!(bookmark.has_remote);
    assert!(bookmark.is_synced);
}

#[test]
fn test_git_push_rejects_stale_remote_lease_without_updating_tracking() {
    let repo = TempJjRepo::new();
    repo.commit("Add feature A");
    repo.create_bookmark("feat-a");
    let (_remote_dir, remote_path) = TempJjRepo::create_bare_remote();
    repo.add_remote("origin", &remote_path);

    let mut workspace = repo.workspace();
    workspace
        .git_push("feat-a", "origin")
        .expect("initial push should succeed");
    let original_remote_oid = bare_remote_bookmark_oid(&remote_path, "feat-a");

    let tree_oid = StdCommand::new("git")
        .args(["rev-parse", "refs/heads/feat-a^{tree}"])
        .current_dir(&remote_path)
        .output()
        .expect("read remote tree");
    assert!(tree_oid.status.success(), "read remote tree");
    let tree_oid = String::from_utf8_lossy(&tree_oid.stdout).trim().to_string();
    let remote_commit = StdCommand::new("git")
        .args([
            "-c",
            "user.name=Remote User",
            "-c",
            "user.email=remote@example.com",
            "commit-tree",
            &tree_oid,
            "-p",
            &original_remote_oid,
            "-m",
            "Advance remote",
        ])
        .current_dir(&remote_path)
        .output()
        .expect("create remote commit");
    assert!(
        remote_commit.status.success(),
        "create remote commit: {}",
        String::from_utf8_lossy(&remote_commit.stderr)
    );
    let advanced_remote_oid = String::from_utf8_lossy(&remote_commit.stdout)
        .trim()
        .to_string();
    let update_remote = StdCommand::new("git")
        .args([
            "update-ref",
            "refs/heads/feat-a",
            &advanced_remote_oid,
            &original_remote_oid,
        ])
        .current_dir(&remote_path)
        .output()
        .expect("advance remote bookmark");
    assert!(
        update_remote.status.success(),
        "advance remote bookmark: {}",
        String::from_utf8_lossy(&update_remote.stderr)
    );

    repo.commit("Advance local");
    repo.move_bookmark("feat-a", "@-");
    let mut workspace = repo.workspace();
    let local_commit_id = workspace
        .get_local_bookmark("feat-a")
        .expect("read local bookmark")
        .expect("feat-a bookmark should exist")
        .commit_id;

    let error = workspace
        .git_push("feat-a", "origin")
        .expect_err("stale remote lease should reject push");
    let error_message = error.to_string();
    assert!(error_message.contains("lease"), "{error_message}");
    assert!(
        error_message.contains("refs/heads/feat-a"),
        "{error_message}"
    );
    assert_eq!(
        bare_remote_bookmark_oid(&remote_path, "feat-a"),
        advanced_remote_oid
    );

    let remote_bookmark = workspace
        .get_remote_bookmark("feat-a", "origin")
        .expect("read tracked remote bookmark")
        .expect("tracked remote bookmark should exist");
    assert_eq!(remote_bookmark.commit_id, original_remote_oid);
    assert_ne!(remote_bookmark.commit_id, local_commit_id);
    assert_eq!(
        remote_bookmark_state(repo.path(), "feat-a", "origin"),
        jj_lib::op_store::RemoteRefState::Tracked
    );
}

/// Test that `git_fetch` handles rewrites after fetching rebased commits.
///
/// This reproduces issue #8 where `ryu sync` panicked with:
/// "BUG: Descendants have not been rebased after the last rewrites"
///
/// The scenario:
/// 1. User creates a bookmark and pushes to remote
/// 2. Remote rebases the commit (e.g., GitHub rebase merge)
/// 3. User fetches - jj detects the rewrite
/// 4. Without calling `rebase_descendants()`, `tx.commit()` panics
#[test]
fn test_git_fetch_handles_rebased_commits() {
    // Create a bare git repo to act as "remote"
    let (_remote_dir, remote_path) = TempJjRepo::create_bare_remote();

    // Create local jj repo
    let repo = TempJjRepo::new();

    // Add the bare repo as a remote
    repo.add_remote("origin", &remote_path);

    // Create a bookmark and push it
    // We need to create the bookmark on the commit with the description, not the working copy
    // jj commit -m "X" creates commit X and leaves WC as empty child
    // So we commit, then move bookmark to parent (@-)
    repo.commit("Add feature A");
    // Create bookmark on the parent (the actual commit with description)
    StdCommand::new("jj")
        .args(["bookmark", "create", "feat-a", "-r", "@-"])
        .current_dir(repo.path())
        .output()
        .expect("create bookmark");
    repo.push_bookmark("feat-a", "origin");

    // Simulate a rebase on the remote by creating a new commit with same content
    // but different commit ID (like GitHub rebase merge does)
    //
    // We do this by:
    // 1. Clone the bare repo to a temp location
    // 2. Checkout the branch
    // 3. Amend the commit to change its ID
    // 4. Force push to the bare repo
    let temp_clone = TempDir::new().expect("create temp clone dir");
    let clone_output = StdCommand::new("git")
        .args(["clone", &remote_path.to_string_lossy(), "."])
        .current_dir(temp_clone.path())
        .output()
        .expect("git clone failed");

    assert!(
        clone_output.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&clone_output.stderr)
    );

    // Checkout the feat-a branch
    let checkout_output = StdCommand::new("git")
        .args(["checkout", "feat-a"])
        .current_dir(temp_clone.path())
        .output()
        .expect("git checkout failed");

    assert!(
        checkout_output.status.success(),
        "git checkout failed: {}",
        String::from_utf8_lossy(&checkout_output.stderr)
    );

    // Configure git user for the clone
    StdCommand::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(temp_clone.path())
        .output()
        .expect("git config email");
    StdCommand::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(temp_clone.path())
        .output()
        .expect("git config name");

    // Create a new commit with --amend to change the commit ID
    // This simulates what happens when GitHub does a rebase merge
    let amend_output = StdCommand::new("git")
        .args([
            "commit",
            "--amend",
            "--allow-empty",
            "-m",
            "Add feature A (rebased)",
            "--date",
            "2026-01-01T00:00:00",
        ])
        .current_dir(temp_clone.path())
        .output()
        .expect("git amend failed");

    assert!(
        amend_output.status.success(),
        "git commit --amend failed: {}",
        String::from_utf8_lossy(&amend_output.stderr)
    );

    // Force push the amended commit to the bare repo
    let push_output = StdCommand::new("git")
        .args(["push", "--force", "origin", "feat-a"])
        .current_dir(temp_clone.path())
        .output()
        .expect("git push failed");

    assert!(
        push_output.status.success(),
        "git push --force failed: {}",
        String::from_utf8_lossy(&push_output.stderr)
    );

    // Now fetch from the remote - this should NOT panic
    // Before the fix, this would panic with:
    // "BUG: Descendants have not been rebased after the last rewrites"
    let mut workspace = repo.workspace();
    let result = workspace.git_fetch("origin");

    assert!(
        result.is_ok(),
        "git_fetch should succeed after remote rebase, got: {:?}",
        result.err()
    );
}

use std::process::Command as StdCommand;
use tempfile::TempDir;

// =============================================================================
// Merged-layer sync (apply_merged_layers)
// =============================================================================

/// Simulates the post-merge state: the bottom PR merged upstream and main
/// advanced (squash). `apply_merged_layers` must drop the merged layer and
/// rebase the remaining stack onto the new trunk tip.
#[test]
fn test_apply_merged_layers_drops_and_rebases() {
    use jj_ryu::sync_merged::{MergedLayer, apply_merged_layers};
    use jj_ryu::tracking::{PrCache, TrackedBookmark, TrackingState};

    let (_remote_dir, remote_path) = TempJjRepo::create_bare_remote();
    let repo = TempJjRepo::new();
    repo.add_remote("origin", &remote_path);

    // main on the initial commit, pushed so trunk() resolves to main@origin
    StdCommand::new("jj")
        .args(["bookmark", "create", "main", "-r", "@-"])
        .current_dir(repo.path())
        .output()
        .expect("create main");
    repo.push_bookmark("main", "origin");

    // Build the stack
    repo.build_stack(&[
        ("feat-a", "Add A"),
        ("feat-b", "Add B"),
        ("feat-c", "Add C"),
    ]);

    // Simulate the squash merge of feat-a landing on main
    StdCommand::new("jj")
        .args(["new", "main", "-m", "Squash merge of feat-a"])
        .current_dir(repo.path())
        .output()
        .expect("jj new main");
    repo.move_bookmark("main", "@");
    repo.push_bookmark("main", "origin");
    StdCommand::new("jj")
        .args(["new", "feat-c"])
        .current_dir(repo.path())
        .output()
        .expect("jj new feat-c");

    let mut workspace = repo.workspace();

    let mut tracking = TrackingState::default();
    for name in ["feat-a", "feat-b", "feat-c"] {
        tracking.track(TrackedBookmark::new(name.to_string(), "change".to_string()));
    }
    let mut pr_cache = PrCache::new();
    pr_cache.upsert("feat-a", &make_pr(1, "feat-a", "main"), "origin");
    pr_cache.upsert("feat-b", &make_pr(2, "feat-b", "feat-a"), "origin");
    pr_cache.upsert("feat-c", &make_pr(3, "feat-c", "feat-b"), "origin");

    let merged = vec![MergedLayer {
        bookmark: "feat-a".to_string(),
        pr_number: Some(1),
    }];
    let reparented =
        apply_merged_layers(&mut workspace, &mut tracking, &mut pr_cache, &merged).unwrap();

    // Merged layer is gone
    assert!(workspace.get_local_bookmark("feat-a").unwrap().is_none());
    assert!(!tracking.is_tracked("feat-a"));
    assert!(pr_cache.get("feat-a").is_none());

    // The remaining stack was reparented onto the new trunk tip
    assert_eq!(reparented, 1);
    let trunk = workspace.resolve_revset("trunk()").unwrap();
    let roots = workspace.resolve_revset("roots(trunk()..@)").unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].parents, vec![trunk[0].commit_id.clone()]);

    // Remaining stack is intact
    let graph = build_change_graph(&workspace).expect("build graph");
    let stack = graph.stack.expect("stack");
    let names: Vec<&str> = stack
        .segments
        .iter()
        .map(|s| s.bookmarks[0].name.as_str())
        .collect();
    assert_eq!(names, ["feat-b", "feat-c"]);
}

/// A chained PR can merge into its open base branch instead of trunk.
/// Dropping the merged layer must keep every commit the open base needs.
#[test]
fn test_apply_merged_layers_keeps_open_base_commits() {
    use jj_ryu::sync_merged::{MergedLayer, apply_merged_layers};
    use jj_ryu::tracking::{PrCache, TrackedBookmark, TrackingState};

    let repo = TempJjRepo::new();
    StdCommand::new("jj")
        .args(["bookmark", "create", "main", "-r", "@-"])
        .current_dir(repo.path())
        .output()
        .expect("create main");
    repo.build_stack(&[("feat-a", "Add A"), ("feat-b", "Add B")]);
    let mut workspace = repo.workspace();
    let feat_a = workspace.get_local_bookmark("feat-a").unwrap().unwrap();

    let mut tracking = TrackingState::default();
    for name in ["feat-a", "feat-b"] {
        tracking.track(TrackedBookmark::new(name.to_string(), "change".to_string()));
    }
    let mut pr_cache = PrCache::new();
    pr_cache.upsert("feat-a", &make_pr(1, "feat-a", "main"), "origin");
    pr_cache.upsert("feat-b", &make_pr(2, "feat-b", "feat-a"), "origin");
    let merged = vec![MergedLayer {
        bookmark: "feat-b".to_string(),
        pr_number: Some(2),
    }];
    apply_merged_layers(&mut workspace, &mut tracking, &mut pr_cache, &merged).unwrap();

    let after = workspace.get_local_bookmark("feat-a").unwrap().unwrap();
    assert_eq!(after.change_id, feat_a.change_id);
    assert!(
        !workspace
            .resolve_revset("feat-a ~ ::trunk()")
            .unwrap()
            .is_empty(),
        "feat-a's commits were abandoned"
    );
    assert!(tracking.is_tracked("feat-a"));
    assert!(!tracking.is_tracked("feat-b"));
}

/// Cleanup must keep ancestors used by an unmerged alias at the merged tip.
#[test]
fn test_apply_merged_layers_keeps_unmerged_alias() {
    use jj_ryu::sync_merged::{MergedLayer, apply_merged_layers};
    use jj_ryu::tracking::{PrCache, TrackedBookmark, TrackingState};

    let repo = TempJjRepo::new();
    repo.build_stack(&[("feat-a", "Add A"), ("feat-b", "Add B")]);
    repo.run_jj(&["bookmark", "create", "alias", "-r", "feat-b"]);
    let mut workspace = repo.workspace();
    let original = workspace.get_local_bookmark("alias").unwrap().unwrap();
    let mut tracking = TrackingState::default();
    tracking.track(TrackedBookmark::new(
        "alias".into(),
        original.change_id.clone(),
    ));
    let mut pr_cache = PrCache::new();

    apply_merged_layers(
        &mut workspace,
        &mut tracking,
        &mut pr_cache,
        &[MergedLayer {
            bookmark: "feat-b".into(),
            pr_number: Some(2),
        }],
    )
    .unwrap();

    let after = workspace.get_local_bookmark("alias").unwrap().unwrap();
    assert_eq!(after.change_id, original.change_id);
    assert_eq!(after.commit_id, original.commit_id);
    assert!(tracking.is_tracked("alias"));
}

/// Detecting merged layers in tracking order can put the upper layer first.
#[test]
fn test_apply_merged_layers_accepts_top_down_order() {
    use jj_ryu::sync_merged::{MergedLayer, apply_merged_layers};
    use jj_ryu::tracking::{PrCache, TrackingState};

    let repo = TempJjRepo::new();
    repo.build_stack(&[
        ("feat-a", "Add A"),
        ("feat-b", "Add B"),
        ("feat-c", "Add C"),
    ]);
    let mut workspace = repo.workspace();
    let original = workspace.get_local_bookmark("feat-c").unwrap().unwrap();
    let mut tracking = TrackingState::default();
    let mut pr_cache = PrCache::new();
    let merged = ["feat-b", "feat-a"].map(|name| MergedLayer {
        bookmark: name.into(),
        pr_number: None,
    });
    apply_merged_layers(&mut workspace, &mut tracking, &mut pr_cache, &merged).unwrap();

    assert!(workspace.get_local_bookmark("feat-a").unwrap().is_none());
    assert!(workspace.get_local_bookmark("feat-b").unwrap().is_none());
    let after = workspace.get_local_bookmark("feat-c").unwrap().unwrap();
    assert_eq!(after.change_id, original.change_id);
    let trunk = workspace.resolve_revset("trunk()").unwrap();
    let remaining = workspace.resolve_revset("trunk()..feat-c").unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].parents, vec![trunk[0].commit_id.clone()]);
}

/// Fetch and drop one merged layer the way `ryu sync` does
fn sync_merged_layer(
    repo: &TempJjRepo,
    tracking: &mut jj_ryu::tracking::TrackingState,
    pr_cache: &mut jj_ryu::tracking::PrCache,
    bookmark: &str,
    pr_number: u64,
) -> jj_ryu::repo::JjWorkspace {
    use jj_ryu::sync_merged::{MergedLayer, apply_merged_layers};

    let mut workspace = repo.workspace();
    workspace.git_fetch("origin").expect("fetch");
    let merged = vec![MergedLayer {
        bookmark: bookmark.to_string(),
        pr_number: Some(pr_number),
    }];
    apply_merged_layers(&mut workspace, tracking, pr_cache, &merged).expect("apply");
    workspace
        .update_working_copy()
        .expect("update working copy");
    workspace
}

/// Reproduces a native-stack merge of the bottom layer: main gains an
/// unrelated commit, GitHub squash-merges feat-a, deletes its branch, and
/// rebases feat-b server-side. The working
/// copy sits on top of feat-b with edits that jj has not snapshotted yet.
/// After sync the working copy must stay on top of feat-b, keep the edits,
/// and not be stale. After the last layer merges, the working copy must
/// move to the new trunk.
#[test]
fn test_sync_keeps_working_copy_on_server_rebased_stack() {
    use jj_ryu::tracking::{PrCache, TrackedBookmark, TrackingState};

    let (_remote_dir, remote_path) = TempJjRepo::create_bare_remote();
    let repo = TempJjRepo::new();
    repo.add_remote("origin", &remote_path);
    repo.run_jj(&["bookmark", "create", "main", "-r", "@-"]);
    repo.push_bookmark("main", "origin");

    for (name, file) in [("feat-a", "a.txt"), ("feat-b", "b.txt")] {
        std::fs::write(repo.path().join(file), name).expect("write layer file");
        repo.run_jj(&["commit", "-m", name]);
        repo.run_jj(&["bookmark", "create", name, "-r", "@-"]);
        repo.push_bookmark(name, "origin");
    }
    std::fs::write(repo.path().join("wip.txt"), "wip").expect("write wip");

    let server = TempDir::new().expect("create server clone dir");
    let git = |args: &[&str]| {
        let output = StdCommand::new("git")
            .args(["-c", "user.name=GitHub", "-c", "user.email=gh@test.com"])
            .args(args)
            .current_dir(server.path())
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["clone", &remote_path.to_string_lossy(), "."]);
    git(&["checkout", "-B", "main", "origin/main"]);
    std::fs::write(server.path().join("other.txt"), "other").expect("write other");
    git(&["add", "other.txt"]);
    git(&["commit", "-m", "Unrelated change on main"]);
    git(&["merge", "--squash", "origin/feat-a"]);
    git(&["commit", "-m", "feat-a (#1)"]);
    git(&["checkout", "-B", "feat-b", "origin/feat-b"]);
    git(&["rebase", "--onto", "main", "origin/feat-a"]);
    git(&["push", "--force", "origin", "main", "feat-b", ":feat-a"]);

    let mut tracking = TrackingState::default();
    for name in ["feat-a", "feat-b"] {
        tracking.track(TrackedBookmark::new(name.to_string(), "change".to_string()));
    }
    let mut pr_cache = PrCache::new();
    pr_cache.upsert("feat-a", &make_pr(1, "feat-a", "main"), "origin");
    pr_cache.upsert("feat-b", &make_pr(2, "feat-b", "feat-a"), "origin");
    let workspace = sync_merged_layer(&repo, &mut tracking, &mut pr_cache, "feat-a", 1);

    let feat_b = workspace.resolve_revset("feat-b").unwrap();
    let feat_b_remote = workspace.resolve_revset("feat-b@origin").unwrap();
    let wc = workspace.resolve_revset("@").unwrap();
    assert_eq!(feat_b[0].commit_id, feat_b_remote[0].commit_id);
    assert_eq!(wc[0].parents, vec![feat_b[0].commit_id.clone()]);

    let status = repo.run_jj(&["status"]);
    assert!(status.contains("wip.txt"), "wip edits lost: {status}");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("wip.txt")).unwrap(),
        "wip"
    );
    assert!(repo.path().join("other.txt").exists());

    let graph = build_change_graph(&workspace).expect("build graph");
    let stack = graph.stack.expect("stack");
    let names: Vec<&str> = stack
        .segments
        .iter()
        .map(|s| s.bookmarks[0].name.as_str())
        .collect();
    assert_eq!(names, ["feat-b"]);

    // Merge the last layer: no native stack remains, and GitHub deletes the
    // branch. The working copy must land on the new trunk.
    git(&["checkout", "main"]);
    git(&["merge", "--squash", "origin/feat-b"]);
    git(&["commit", "-m", "feat-b (#2)"]);
    git(&["push", "origin", "main", ":feat-b"]);

    let workspace = sync_merged_layer(&repo, &mut tracking, &mut pr_cache, "feat-b", 2);

    let trunk = workspace.resolve_revset("trunk()").unwrap();
    let wc = workspace.resolve_revset("@").unwrap();
    assert_eq!(wc[0].parents, vec![trunk[0].commit_id.clone()]);
    let status = repo.run_jj(&["status"]);
    assert!(status.contains("wip.txt"), "wip edits lost: {status}");
    assert!(repo.path().join("b.txt").exists());
    assert!(build_change_graph(&workspace).unwrap().stack.is_none());
}

/// Only commits that a selected bookmark pushes may block a submission
#[test]
fn test_ensure_no_conflicts_checks_pushed_range() {
    use jj_ryu::error::Error;
    use jj_ryu::submit::ensure_no_conflicts;

    let repo = TempJjRepo::new();
    for (name, content) in [("feat-a", "one"), ("feat-b", "two")] {
        std::fs::write(repo.path().join("x.txt"), content).expect("write x");
        repo.run_jj(&["commit", "-m", name]);
        repo.run_jj(&["bookmark", "create", name, "-r", "@-"]);
    }
    std::fs::write(repo.path().join("x.txt"), "three").expect("write x");

    // Rewrite feat-b: the working copy above it conflicts, feat-b does not
    repo.run_jj(&["edit", "feat-b"]);
    std::fs::write(repo.path().join("x.txt"), "four").expect("write x");
    repo.run_jj(&["new", "feat-b"]);
    repo.run_jj(&[
        "edit",
        "description(exact:\"\") & children(feat-b) & conflicts()",
    ]);

    let workspace = repo.workspace();
    let graph = build_change_graph(&workspace).expect("build graph");
    let analysis = analyze_submission(&graph, None).expect("analyze");
    assert!(workspace.has_conflicts("@").unwrap());
    ensure_no_conflicts(&workspace, &analysis).expect("conflict above feat-b must not block");

    // Rewrite feat-a: feat-b now conflicts and blocks, feat-a alone does not
    repo.run_jj(&["edit", "feat-a"]);
    std::fs::write(repo.path().join("x.txt"), "five").expect("write x");
    repo.run_jj(&["new", "feat-b"]);

    let workspace = repo.workspace();
    let graph = build_change_graph(&workspace).expect("build graph");
    let analysis = analyze_submission(&graph, None).expect("analyze");
    assert!(matches!(
        ensure_no_conflicts(&workspace, &analysis),
        Err(Error::ConflictedCommit(name)) if name == "feat-b"
    ));
    let lower = analyze_submission(&graph, Some("feat-a")).expect("analyze");
    ensure_no_conflicts(&workspace, &lower).expect("feat-a is clean");
}

/// A rebase merge lands copies of every commit in the layer on trunk. Sync
/// must abandon all local commits of the layer, not only its tip, or the
/// older commits come back as duplicates on top of trunk.
#[test]
fn test_sync_abandons_every_commit_of_rebase_merged_layer() {
    use jj_ryu::tracking::{PrCache, TrackedBookmark, TrackingState};

    let (_remote_dir, remote_path) = TempJjRepo::create_bare_remote();
    let repo = TempJjRepo::new();
    repo.add_remote("origin", &remote_path);
    repo.run_jj(&["bookmark", "create", "main", "-r", "@-"]);
    repo.push_bookmark("main", "origin");

    for (file, message) in [("a1.txt", "feat-a part 1"), ("a2.txt", "feat-a part 2")] {
        std::fs::write(repo.path().join(file), message).expect("write layer file");
        repo.run_jj(&["commit", "-m", message]);
    }
    repo.run_jj(&["bookmark", "create", "feat-a", "-r", "@-"]);
    repo.push_bookmark("feat-a", "origin");
    std::fs::write(repo.path().join("wip.txt"), "wip").expect("write wip");

    let server = TempDir::new().expect("create server clone dir");
    let git = |args: &[&str]| {
        let output = StdCommand::new("git")
            .args(["-c", "user.name=GitHub", "-c", "user.email=gh@test.com"])
            .args(args)
            .current_dir(server.path())
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["clone", &remote_path.to_string_lossy(), "."]);
    git(&["checkout", "-B", "main", "origin/main"]);
    std::fs::write(server.path().join("other.txt"), "other").expect("write other");
    git(&["add", "other.txt"]);
    git(&["commit", "-m", "Unrelated change on main"]);
    git(&["checkout", "-B", "feat-a", "origin/feat-a"]);
    git(&["rebase", "main"]);
    git(&["checkout", "main"]);
    git(&["merge", "--ff-only", "feat-a"]);
    git(&["push", "origin", "main"]);

    let mut tracking = TrackingState::default();
    tracking.track(TrackedBookmark::new(
        "feat-a".to_string(),
        "change".to_string(),
    ));
    let mut pr_cache = PrCache::new();
    pr_cache.upsert("feat-a", &make_pr(1, "feat-a", "main"), "origin");
    let workspace = sync_merged_layer(&repo, &mut tracking, &mut pr_cache, "feat-a", 1);

    let trunk = workspace.resolve_revset("trunk()").unwrap();
    let wc = workspace.resolve_revset("@").unwrap();
    assert_eq!(wc[0].parents, vec![trunk[0].commit_id.clone()]);
    assert_eq!(workspace.resolve_revset("trunk()..@").unwrap().len(), 1);
    let status = repo.run_jj(&["status"]);
    assert!(status.contains("wip.txt"), "wip edits lost: {status}");
    assert!(
        !status.contains("conflict"),
        "conflicts after sync: {status}"
    );
}

// =============================================================================
// Colocated Git ref export
// =============================================================================

/// Git branch target in the colocated repo, read without running jj
fn colocated_git_branch(repo_path: &std::path::Path, branch: &str) -> Option<String> {
    let output = StdCommand::new("git")
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .current_dir(repo_path)
        .output()
        .expect("read colocated git branch");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn assert_git_branch_follows_bookmark(
    repo_path: &std::path::Path,
    workspace: &jj_ryu::repo::JjWorkspace,
    name: &str,
) {
    let bookmark = workspace.get_local_bookmark(name).unwrap().unwrap();
    assert_eq!(
        colocated_git_branch(repo_path, name),
        Some(bookmark.commit_id),
        "git branch {name} is stale"
    );
}

/// Ryu drives jj-lib directly, so it must export bookmarks to the colocated
/// Git repo the way the jj CLI does after each operation.
#[test]
fn test_git_fetch_exports_fast_forwarded_bookmark_to_git() {
    let (_remote_dir, remote_path) = TempJjRepo::create_bare_remote();
    let repo = TempJjRepo::new();
    repo.add_remote("origin", &remote_path);
    repo.run_jj(&["bookmark", "create", "main", "-r", "@-"]);
    repo.push_bookmark("main", "origin");

    let server = TempDir::new().expect("create server clone dir");
    let git = |args: &[&str]| {
        let output = StdCommand::new("git")
            .args(["-c", "user.name=GitHub", "-c", "user.email=gh@test.com"])
            .args(args)
            .current_dir(server.path())
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["clone", &remote_path.to_string_lossy(), "."]);
    git(&["checkout", "-B", "main", "origin/main"]);
    git(&["commit", "--allow-empty", "-m", "Merge on the server"]);
    git(&["push", "origin", "main"]);

    let before = colocated_git_branch(repo.path(), "main");
    let mut workspace = repo.workspace();
    workspace.git_fetch("origin").expect("fetch");

    let bookmark = workspace.get_local_bookmark("main").unwrap().unwrap();
    assert_ne!(
        Some(bookmark.commit_id),
        before,
        "fetch did not advance main"
    );
    assert_git_branch_follows_bookmark(repo.path(), &workspace, "main");
}

/// Deleting, abandoning, and rebasing in `apply_merged_layers` each rewrite
/// bookmarks and the working copy, so the Git branches and `HEAD` must follow.
#[test]
fn test_apply_merged_layers_exports_bookmarks_to_git() {
    use jj_ryu::sync_merged::{MergedLayer, apply_merged_layers};
    use jj_ryu::tracking::{PrCache, TrackingState};

    let (_remote_dir, remote_path) = TempJjRepo::create_bare_remote();
    let repo = TempJjRepo::new();
    repo.add_remote("origin", &remote_path);
    repo.run_jj(&["bookmark", "create", "main", "-r", "@-"]);
    repo.push_bookmark("main", "origin");
    repo.build_stack(&[
        ("feat-a", "Add A"),
        ("feat-b", "Add B"),
        ("feat-c", "Add C"),
    ]);
    repo.run_jj(&["new", "main", "-m", "Squash merge of feat-a"]);
    repo.move_bookmark("main", "@");
    repo.push_bookmark("main", "origin");
    repo.run_jj(&["new", "feat-c"]);

    let mut workspace = repo.workspace();
    let feat_b_before = colocated_git_branch(repo.path(), "feat-b");
    let mut tracking = TrackingState::default();
    let mut pr_cache = PrCache::new();
    let merged = vec![MergedLayer {
        bookmark: "feat-a".to_string(),
        pr_number: None,
    }];
    apply_merged_layers(&mut workspace, &mut tracking, &mut pr_cache, &merged).unwrap();

    let git_head = StdCommand::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("read git HEAD");
    let wc_parent = workspace.resolve_revset("@-").unwrap();
    assert_eq!(
        String::from_utf8_lossy(&git_head.stdout).trim(),
        wc_parent[0].commit_id,
        "git HEAD is stale"
    );
    assert_eq!(colocated_git_branch(repo.path(), "feat-a"), None);
    let feat_b = workspace.get_local_bookmark("feat-b").unwrap().unwrap();
    assert_ne!(
        Some(feat_b.commit_id),
        feat_b_before,
        "feat-b was not rebased"
    );
    assert_git_branch_follows_bookmark(repo.path(), &workspace, "feat-b");
    assert_git_branch_follows_bookmark(repo.path(), &workspace, "feat-c");
}
