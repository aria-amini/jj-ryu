//! Sync command - sync current stack with remote and prune landed bookmarks

use crate::cli::CliProgress;
use crate::cli::style::{CHECK, Stylize, arrow, check, spinner_style};
use anstream::println;
use dialoguer::Confirm;
use indicatif::ProgressBar;
use jj_ryu::error::{Error, Result};
use jj_ryu::graph::build_change_graph;
use jj_ryu::platform::{create_platform_service, parse_repo_info};
use jj_ryu::repo::{JjWorkspace, select_remote};
use jj_ryu::submit::{
    SubmissionPlan, analyze_submission, create_submission_plan, ensure_no_conflicts,
    execute_submission,
};
use jj_ryu::sync_merged::{PrunePlan, apply_prune, collect_prune_input, plan_prune};
use jj_ryu::tracking::{load_pr_cache, load_tracking, save_pr_cache, save_tracking};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

/// Options for the sync command
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    /// Dry run - show what would be done without making changes
    pub dry_run: bool,
    /// Preview plan and prompt for confirmation before executing
    pub confirm: bool,
    /// Sync all bookmarks in `trunk()`..@ (ignore tracking)
    pub all: bool,
    /// Also delete bookmarks whose PR was closed without merging
    pub prune_closed: bool,
}

/// Run the sync command
#[allow(clippy::too_many_lines)]
pub async fn run_sync(path: &Path, remote: Option<&str>, options: SyncOptions) -> Result<()> {
    // Open workspace
    let mut workspace = JjWorkspace::open(path)?;
    let workspace_root = workspace.workspace_root().to_path_buf();

    // Load tracking state (unless --all bypasses tracking)
    let mut tracking = load_tracking(&workspace_root)?;
    let mut tracked_names: Vec<String> = tracking
        .tracked_names()
        .into_iter()
        .map(str::to_string)
        .collect();

    // Get remotes and select one
    let remotes = workspace.git_remotes()?;
    let remote_name = select_remote(&remotes, remote)?;

    // Detect platform
    let remote_info = remotes
        .iter()
        .find(|r| r.name == remote_name)
        .ok_or_else(|| Error::RemoteNotFound(remote_name.clone()))?;

    let platform_config = parse_repo_info(&remote_info.url)?;

    // Create platform service
    let platform = create_platform_service(&platform_config).await?;

    // Fetch from remote with spinner
    if !options.dry_run {
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(spinner_style());
        spinner.set_message(format!("Fetching from {}...", remote_name.emphasis()));
        spinner.enable_steady_tick(Duration::from_millis(80));

        workspace.git_fetch(&remote_name)?;

        spinner.finish_with_message(format!(
            "{} Fetched from {}",
            check(),
            remote_name.emphasis()
        ));
    }

    // Prune landed bookmarks before planning. This covers every local
    // bookmark, not only tracked ones in the current stack, so it also works
    // from an empty working copy at trunk.
    let mut pr_cache = load_pr_cache(&workspace_root).unwrap_or_default();
    let input = collect_prune_input(&workspace, &tracking)?;
    let plan = plan_prune(platform.as_ref(), input).await?;
    let had_tracked = !tracked_names.is_empty();
    let removed = prune_stale(
        &mut workspace,
        &mut tracking,
        &mut pr_cache,
        &plan,
        &options,
    )?;
    let Some(removed) = removed else {
        return Ok(());
    };
    if !options.dry_run && !removed.is_empty() {
        save_tracking(&workspace_root, &tracking)?;
        save_pr_cache(&workspace_root, &pr_cache)?;
    }
    tracked_names.retain(|name| !removed.contains(name.as_str()));

    if !options.dry_run {
        workspace.update_working_copy()?;
    }

    if tracked_names.is_empty() && !options.all {
        if had_tracked {
            println!("{}", "Entire stack merged; nothing left to sync.".muted());
        } else {
            println!(
                "{}",
                "No bookmarks tracked. Run 'ryu track' first, or use 'ryu sync --all' to sync all bookmarks."
                    .muted()
            );
        }
        return Ok(());
    }

    // Build change graph from working copy
    let graph = build_change_graph(&workspace)?;

    if graph.stack.is_none() {
        println!("{}", "No stack to sync".muted());
        println!(
            "{}",
            "Create bookmarks between trunk and working copy first.".muted()
        );
        return Ok(());
    }

    let default_branch = workspace.default_branch()?;
    let progress = CliProgress::compact();

    // Analyze and plan for the single stack
    let mut analysis = analyze_submission(&graph, None)?;

    // Filter to tracked bookmarks unless --all
    if !options.all && !tracked_names.is_empty() {
        analysis
            .segments
            .retain(|s| tracked_names.iter().any(|n| n == &s.bookmark.name));
        if analysis.segments.is_empty() {
            return Err(Error::Tracking(
                "No tracked bookmarks in stack. Use 'ryu track' to track bookmarks, or 'ryu sync --all'.".to_string()
            ));
        }
    }

    ensure_no_conflicts(&workspace, &analysis)?;

    let plan =
        create_submission_plan(&analysis, platform.as_ref(), &remote_name, &default_branch).await?;

    // Show confirmation if requested
    if options.confirm && !options.dry_run {
        print_sync_preview(&plan);
        if !Confirm::new()
            .with_prompt("Proceed with sync?")
            .default(true)
            .interact()
            .map_err(|e| Error::Internal(format!("Failed to read confirmation: {e}")))?
        {
            println!("{}", "Aborted".muted());
            return Ok(());
        }
        println!();
    }

    // Execute
    println!(
        "{} {}",
        "Syncing stack:".emphasis(),
        analysis.target_bookmark.accent()
    );

    let result = execute_submission(
        &plan,
        &mut workspace,
        platform.as_ref(),
        &progress,
        options.dry_run,
    )
    .await?;

    // Summary
    println!();
    if options.dry_run {
        println!("{}", "Dry run complete".muted());
    } else {
        println!(
            "{} {} pushed, {} created, {} updated",
            format!("{CHECK} Sync complete:").success(),
            result.pushed_bookmarks.len().accent(),
            result.created_prs.len().accent(),
            result.updated_prs.len().accent()
        );
    }

    Ok(())
}

/// Describe bookmarks as `name (#pr), ...`
fn describe<'a>(items: impl IntoIterator<Item = (&'a str, Option<u64>)>) -> String {
    items
        .into_iter()
        .map(|(name, pr)| pr.map_or_else(|| name.to_string(), |n| format!("{name} (#{n})")))
        .collect::<Vec<_>>()
        .join(", ")
}

const fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Report and apply the prune plan
///
/// Returns the bookmark names removed from tracking (or that a dry run would
/// remove), or `None` if the user declined the confirmation.
fn prune_stale(
    workspace: &mut JjWorkspace,
    tracking: &mut jj_ryu::tracking::TrackingState,
    pr_cache: &mut jj_ryu::tracking::PrCache,
    plan: &PrunePlan,
    options: &SyncOptions,
) -> Result<Option<HashSet<String>>> {
    let merged = describe(
        plan.merged
            .iter()
            .map(|m| (m.bookmark.as_str(), m.pr_number)),
    );
    let closed = describe(
        plan.closed
            .iter()
            .map(|c| (c.bookmark.as_str(), Some(c.pr_number))),
    );
    let diverged = describe(
        plan.diverged
            .iter()
            .map(|d| (d.bookmark.as_str(), Some(d.pr_number))),
    );
    let dangling = plan.dangling.join(", ");
    let delete_closed = options.prune_closed && !plan.closed.is_empty();

    if !plan.closed.is_empty() && !options.prune_closed {
        println!(
            "{}",
            format!("Stale (PR closed, kept; use --prune-closed to delete): {closed}").muted()
        );
    }
    if !plan.diverged.is_empty() {
        println!(
            "{}",
            format!("Kept (PR merged, but the bookmark has other commits): {diverged}").muted()
        );
    }

    if options.dry_run || options.confirm {
        let verb = if options.dry_run { "Would" } else { "Will" };
        if !plan.merged.is_empty() {
            println!(
                "{verb} drop {} merged bookmark{}: {merged}",
                plan.merged.len(),
                plural(plan.merged.len())
            );
        }
        if delete_closed {
            println!(
                "{verb} delete {} bookmark{} with a closed PR: {closed}",
                plan.closed.len(),
                plural(plan.closed.len())
            );
        }
        if !plan.dangling.is_empty() {
            println!(
                "{verb} untrack {} bookmark{} that no longer exist{}: {dangling}",
                plan.dangling.len(),
                plural(plan.dangling.len()),
                if plan.dangling.len() == 1 { "s" } else { "" }
            );
        }
    }

    if !options.dry_run
        && options.confirm
        && (!plan.merged.is_empty() || delete_closed)
        && !Confirm::new()
            .with_prompt("Delete these bookmarks?")
            .default(true)
            .interact()
            .map_err(|e| Error::Internal(format!("Failed to read confirmation: {e}")))?
    {
        println!("{}", "Aborted".muted());
        return Ok(None);
    }

    let mut removed: HashSet<String> = plan.merged.iter().map(|m| m.bookmark.clone()).collect();
    removed.extend(plan.dangling.iter().cloned());
    if options.prune_closed {
        removed.extend(plan.closed.iter().map(|c| c.bookmark.clone()));
    }

    if !options.dry_run {
        apply_prune(workspace, tracking, pr_cache, plan, options.prune_closed)?;
        if !plan.merged.is_empty() {
            println!(
                "{} Dropped {} merged bookmark{}: {merged}",
                check(),
                plan.merged.len().to_string().accent(),
                plural(plan.merged.len())
            );
        }
        if delete_closed {
            println!(
                "{} Deleted {} bookmark{} with a closed PR: {closed}",
                check(),
                plan.closed.len().to_string().accent(),
                plural(plan.closed.len())
            );
        }
        if !plan.dangling.is_empty() {
            println!(
                "{} Untracked {} bookmark{} that no longer exist{}: {dangling}",
                check(),
                plan.dangling.len().to_string().accent(),
                plural(plan.dangling.len()),
                if plan.dangling.len() == 1 { "s" } else { "" }
            );
        }
    }
    Ok(Some(removed))
}

/// Print sync preview for --confirm
fn print_sync_preview(plan: &SubmissionPlan) {
    println!("{}:", "Sync plan".emphasis());
    println!();

    if plan.execution_steps.is_empty() {
        println!("  {}", "Already in sync".muted());
        println!();
        return;
    }

    println!("  {}:", "Steps".emphasis());
    for step in &plan.execution_steps {
        println!("    {} {}", arrow(), step);
    }

    println!();
}
