//! Merge command for standalone PRs and native GitHub stacks

use crate::cli::common;
use crate::cli::style::{Stylize, check, spinner_style};
use anstream::println;
use dialoguer::Confirm;
use indicatif::ProgressBar;
use jj_ryu::error::{Error, Result};
use jj_ryu::merge::{MergePlan, MergeTarget, poll_merge, resolve_merge_target};
use jj_ryu::tracking::load_pr_cache;
use jj_ryu::types::{MergeAsyncOutcome, MergeAsyncState, MergeMethod, MergeOptions};
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
            feature: "async merge",
            platform: ctx.platform.config().platform,
        });
    }

    let mut pr_cache = load_pr_cache(&ctx.workspace_root)?;
    pr_cache.prs.retain(|pr| pr.remote == ctx.remote_name);
    let target = resolve_merge_target(ctx.platform.as_ref(), &pr_cache, bookmark).await?;
    let (lookup_pr, plan) = match &target {
        MergeTarget::Single(number) => (*number, None),
        MergeTarget::Stack(plan) => (
            plan.layers
                .last()
                .ok_or_else(|| Error::Internal("empty merge plan".into()))?
                .number,
            Some(plan),
        ),
    };

    let head_sha = if let Some(plan) = plan {
        plan.layers.last().map(|entry| entry.head.sha.clone())
    } else {
        Some(ctx.platform.pr_head_sha(lookup_pr).await?)
    };

    let prompt = plan.map_or_else(
        || {
            println!("Merging PR #{lookup_pr}:");
            format!("Merge PR #{lookup_pr}?")
        },
        |plan| {
            print_merge_plan(plan);
            format!(
                "Merge {} PR(s) from the bottom of stack #{}?",
                plan.layers.len(),
                plan.stack.number
            )
        },
    );

    if !yes
        && !Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()
            .map_err(|e| Error::Internal(format!("Failed to read confirmation: {e}")))?
    {
        println!("{}", "Aborted.".muted());
        return Ok(());
    }

    let options = MergeOptions {
        merge_method: method,
        sha: head_sha,
        ..MergeOptions::default()
    };

    let outcome = ctx
        .platform
        .merge_pr_async(lookup_pr, &options)
        .await
        .map_err(|e| match e {
            Error::MergeInProgress(msg) => Error::MergeInProgress(format!(
                "{msg}; wait for it to finish or check the PR on GitHub"
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

    if let Some(plan) = plan {
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
    } else {
        println!("{} Merged PR #{lookup_pr}", check());
    }
    println!("Run {} to update your local stack.", "ryu sync".accent());

    Ok(())
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
