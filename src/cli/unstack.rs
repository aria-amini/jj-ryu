//! Unstack command - remove PRs from a native GitHub stack

use crate::cli::common;
use crate::cli::style::{Stylize, check};
use anstream::println;
use dialoguer::Confirm;
use jj_ryu::error::{Error, Result};
use jj_ryu::tracking::{load_pr_cache, load_tracking};
use jj_ryu::types::PrStack;
use jj_ryu::unstack::{UnstackOutcome, find_submitted_stack};
use std::path::Path;

/// Run the unstack command
///
/// Finds the native stack containing any submitted PR and removes all its
/// unmerged PRs (GitHub-side grouping only; local bookmarks, branches, and
/// tracking state are untouched).
pub async fn run_unstack(path: &Path, remote: Option<&str>, yes: bool) -> Result<()> {
    let ctx = common::open_platform(path, remote).await?;

    if !ctx.platform.supports_native_stacks() {
        return Err(Error::Unsupported {
            feature: "native stacks",
            platform: ctx.platform.config().platform,
        });
    }

    let pr_cache = load_pr_cache(&ctx.workspace_root).unwrap_or_default();
    let pr_numbers: Vec<u64> = pr_cache.prs.iter().map(|p| p.number).collect();
    let tracking = load_tracking(&ctx.workspace_root).unwrap_or_default();
    if pr_numbers.is_empty() && tracking.bookmarks.is_empty() {
        println!(
            "{}",
            "No submitted PRs found. Run 'ryu submit' first.".muted()
        );
        return Ok(());
    }

    let stack = if pr_numbers.is_empty() {
        None
    } else {
        find_submitted_stack(ctx.platform.as_ref(), &pr_numbers)
            .await
            .map_err(|e| match e {
                Error::StacksUnavailable(msg) => Error::StacksUnavailable(format!(
                    "{msg}; unstack manually from the GitHub UI if needed"
                )),
                other => other,
            })?
    };

    // The pr_cache can be stale (entries pointing at long-closed PRs), so
    // fall back to resolving tracked bookmarks against the platform
    let stack = match stack {
        Some(stack) => Some(stack),
        None => {
            let mut found = None;
            for bookmark in &tracking.bookmarks {
                if let Ok(Some(pr)) = ctx.platform.find_existing_pr(&bookmark.name).await {
                    found = find_submitted_stack(ctx.platform.as_ref(), &[pr.number]).await?;
                    if found.is_some() {
                        break;
                    }
                }
            }
            found
        }
    };

    let Some(stack) = stack else {
        println!("{}", "No submitted PR belongs to a native stack.".muted());
        return Ok(());
    };

    print_stack_summary(&stack, &ctx.remote_name);

    if !yes {
        let prompt = format!("Remove all unmerged PRs from stack #{}?", stack.number);
        if !Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()
            .map_err(|e| Error::Internal(format!("Failed to read confirmation: {e}")))?
        {
            println!("{}", "Aborted.".muted());
            return Ok(());
        }
    }

    match jj_ryu::unstack::unstack(ctx.platform.as_ref(), stack.number).await? {
        UnstackOutcome::Remaining {
            stack_number,
            remaining,
        } => {
            println!(
                "{} Removed unmerged PRs; {remaining} PR(s) remain in stack #{stack_number}",
                check()
            );
        }
        UnstackOutcome::Dissolved { stack_number } => {
            println!("{} Stack #{stack_number} dissolved", check());
        }
    }

    Ok(())
}

/// Print stack number, base, and entry counts
fn print_stack_summary(stack: &PrStack, remote: &str) {
    let merged = stack
        .pull_requests
        .iter()
        .filter(|e| e.merged_at.is_some())
        .count();
    let total = stack.pull_requests.len();
    println!(
        "Stack #{} (remote: {}, base: {}, {} PRs, {} merged)",
        stack.number.to_string().accent(),
        remote,
        stack.base.ref_field,
        total,
        merged
    );
    for entry in &stack.pull_requests {
        let status = if entry.merged_at.is_some() {
            "merged".muted().to_string()
        } else if entry.draft {
            "draft".warn().to_string()
        } else {
            "open".to_string()
        };
        println!("  #{} {} [{}]", entry.number, entry.head.ref_field, status);
    }
}
