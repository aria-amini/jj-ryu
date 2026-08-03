//! Merge command - merge a stacked PR and all PRs below it (GitHub Stacks)

use crate::cli::common;
use crate::cli::style::{Stylize, check, spinner_style};
use anstream::println;
use dialoguer::Confirm;
use indicatif::ProgressBar;
use jj_ryu::error::{Error, Result};
use jj_ryu::merge::{MergePlan, plan_merge, poll_merge};
use jj_ryu::platform::PlatformService;
use jj_ryu::tracking::{PrCache, load_pr_cache, save_pr_cache};
use jj_ryu::types::{MergeAsyncOutcome, MergeAsyncState, MergeMethod, MergeOptions, PrStack};
use std::path::Path;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const POLL_TIMEOUT: Duration = Duration::from_secs(120);

/// Run the merge command
#[allow(clippy::too_many_lines)]
pub async fn run_merge(
    path: &Path,
    bookmark: Option<&str>,
    method: MergeMethod,
    yes: bool,
    remote: Option<&str>,
) -> Result<()> {
    let ctx = common::open_platform(path, remote).await?;

    if !ctx.platform.supports_native_stacks() {
        return Err(Error::Unsupported {
            feature: "native stacked merge",
            platform: ctx.platform.config().platform,
        });
    }

    let mut pr_cache = load_pr_cache(&ctx.workspace_root).unwrap_or_default();

    // Resolve the target PR: explicit bookmark, or bottom unmerged stack layer
    let lookup_pr = match bookmark {
        Some(bm) => resolve_bookmark_pr(ctx.platform.as_ref(), &pr_cache, bm).await?,
        None => bottom_unmerged_pr(ctx.platform.as_ref(), &pr_cache).await?,
    };

    let stack = find_stack(ctx.platform.as_ref(), lookup_pr).await?;
    let plan = plan_merge(stack, lookup_pr)?;

    print_merge_plan(&plan);

    if !yes {
        let prompt = format!(
            "Merge {} PR(s) from the bottom of stack #{}?",
            plan.layers.len(),
            plan.stack.number
        );
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

    let options = MergeOptions {
        merge_method: method,
        sha: plan.layers.last().map(|e| e.head.sha.clone()),
        ..MergeOptions::default()
    };

    let outcome = ctx
        .platform
        .merge_pr_async(lookup_pr, &options)
        .await
        .map_err(|e| match e {
            Error::MergeInProgress(msg) => Error::MergeInProgress(format!(
                "{msg}; wait for it to finish or check the stack on GitHub"
            )),
            other => other,
        })?;

    match outcome {
        MergeAsyncOutcome::AlreadyMergedOrQueued => {
            println!(
                "{}",
                "Already merged or queued for merge; run `ryu sync` to update your local stack."
                    .muted()
            );
            return Ok(());
        }
        MergeAsyncOutcome::Accepted { uuid } => {
            let spinner = ProgressBar::new_spinner();
            spinner.set_style(spinner_style());
            spinner.set_message("Merging...");
            spinner.enable_steady_tick(Duration::from_millis(80));

            let state = poll_merge(
                ctx.platform.as_ref(),
                lookup_pr,
                &uuid,
                POLL_INTERVAL,
                POLL_TIMEOUT,
            )
            .await;

            match state {
                Ok(MergeAsyncState::Merged) => {
                    spinner.finish_with_message(format!("{} Merged", check()));
                }
                Ok(MergeAsyncState::Failed) => {
                    spinner.finish_with_message("Merge failed".to_string());
                    return Err(Error::GitHubApi(format!(
                        "async merge of PR #{lookup_pr} failed; check the PR on GitHub"
                    )));
                }
                Ok(state) => {
                    spinner.finish_with_message(format!("Still processing ({state:?})"));
                    println!(
                        "{}",
                        format!(
                            "Merge is still processing (poll id: {uuid}); check GitHub, then run `ryu sync`."
                        )
                        .muted()
                    );
                    return Ok(());
                }
                Err(e) => {
                    spinner.finish_with_message("Merge status unknown".to_string());
                    return Err(e);
                }
            }
        }
    }

    // Bookkeep merged layers locally
    for layer in &plan.layers {
        pr_cache.remove(&layer.head.ref_field);
    }
    save_pr_cache(&ctx.workspace_root, &pr_cache)?;

    println!(
        "{} Merged {} PR(s) from stack #{}",
        check(),
        plan.layers.len().to_string().accent(),
        plan.stack.number
    );
    if !plan.remaining().is_empty() {
        println!(
            "{}",
            "GitHub is rebasing the rest of the stack server-side.".muted()
        );
    }
    println!("Run {} to update your local stack.", "ryu sync".accent());

    Ok(())
}

/// Resolve a bookmark to its PR number (cache first, then the platform)
async fn resolve_bookmark_pr(
    platform: &dyn PlatformService,
    pr_cache: &PrCache,
    bookmark: &str,
) -> Result<u64> {
    if let Some(cached) = pr_cache.get(bookmark) {
        return Ok(cached.number);
    }
    let pr = platform.find_existing_pr(bookmark).await?.ok_or_else(|| {
        Error::InvalidArgument(format!("no open PR found for bookmark '{bookmark}'"))
    })?;
    Ok(pr.number)
}

/// Find the bottom unmerged entry of the native stack containing any cached PR
async fn bottom_unmerged_pr(platform: &dyn PlatformService, pr_cache: &PrCache) -> Result<u64> {
    let pr_numbers: Vec<u64> = pr_cache.prs.iter().map(|p| p.number).collect();
    if pr_numbers.is_empty() {
        return Err(Error::InvalidArgument(
            "no submitted PRs found; run `ryu submit` first".to_string(),
        ));
    }
    let stack = find_stack(platform, *pr_numbers.first().unwrap()).await?;
    stack
        .pull_requests
        .iter()
        .find(|e| e.merged_at.is_none())
        .map(|e| e.number)
        .ok_or_else(|| {
            Error::InvalidArgument(format!(
                "every PR in stack #{} is already merged; run `ryu sync`",
                stack.number
            ))
        })
}

/// Find the native stack for a PR with actionable error messages
async fn find_stack(platform: &dyn PlatformService, pr_number: u64) -> Result<PrStack> {
    let stack = platform.find_stack_for_pr(pr_number).await.map_err(|e| match e {
        Error::StacksUnavailable(msg) => Error::StacksUnavailable(format!(
            "{msg}; merge the bottom PR with a regular merge (`gh pr merge`) and rebase manually"
        )),
        other => other,
    })?;
    stack.ok_or_else(|| {
        Error::InvalidArgument(format!(
            "PR #{pr_number} is not in a native stack; merge it with `gh pr merge`"
        ))
    })
}

/// Print the layers that will merge and what remains
fn print_merge_plan(plan: &MergePlan) {
    println!(
        "Merging {} PR(s) from the bottom of stack #{}:",
        plan.layers.len().to_string().accent(),
        plan.stack.number
    );
    for layer in &plan.layers {
        println!("  #{} {}", layer.number, layer.head.ref_field);
    }
    let remaining = plan.remaining();
    if !remaining.is_empty() {
        println!(
            "{}",
            format!(
                "The remaining {} PR(s) will be retargeted and rebased by GitHub:",
                remaining.len()
            )
            .muted()
        );
        for entry in remaining {
            println!(
                "{}",
                format!("  #{} {}", entry.number, entry.head.ref_field).muted()
            );
        }
    }
}
