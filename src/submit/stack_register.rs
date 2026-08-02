//! Stack registration phase
//!
//! Registers submitted PRs as a native GitHub stack via the Stacks API.
//! This phase is entirely soft-fail: registration problems never fail a
//! submission whose PRs were pushed/created successfully.

use crate::error::Error;
use crate::platform::PlatformService;
use crate::submit::plan::SubmissionPlan;
use crate::submit::progress::ProgressCallback;
use crate::types::PullRequest;
use std::collections::HashMap;
use tracing::debug;

/// Retry backoffs (ms) for HTTP 409 concurrent-modification responses
const RETRY_BACKOFF_MS: [u64; 3] = [250, 500, 1000];

/// Action to take for native stack registration
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackAction {
    /// No stack exists - create one from these PRs (bottom to top)
    Create {
        /// PR numbers, bottom to top
        prs: Vec<u64>,
    },
    /// Existing stack is a prefix of ours - append the delta
    Add {
        /// Stack number to extend
        stack_number: u64,
        /// PR numbers to append
        delta: Vec<u64>,
    },
    /// Stack already matches exactly
    NoOp {
        /// Stack number
        stack_number: u64,
    },
    /// Registration not applicable
    Skip {
        /// Why registration is skipped
        reason: String,
    },
    /// Local and remote stacks disagree (reorder/removal) - manual fix needed
    ReorderConflict {
        /// Human-readable explanation
        message: String,
    },
}

/// PRs for the submitted bookmarks, ordered bottom to top (plan segment order)
#[allow(clippy::implicit_hasher)]
pub fn ordered_prs(
    plan: &SubmissionPlan,
    bookmark_to_pr: &HashMap<String, PullRequest>,
) -> Vec<PullRequest> {
    plan.segments
        .iter()
        .filter_map(|seg| bookmark_to_pr.get(&seg.bookmark.name).cloned())
        .collect()
}

/// Decide what stack registration should do, mirroring gh-stack's upsert
///
/// Read-only: only calls `find_stack_for_pr`.
///
/// # Errors
/// Returns `Err` only on platform/API errors (including
/// [`Error::StacksUnavailable`]); conflicts are returned as
/// [`StackAction::ReorderConflict`].
pub async fn plan_stack_action(
    platform: &dyn PlatformService,
    prs: &[PullRequest],
) -> crate::error::Result<StackAction> {
    if prs.len() < 2 {
        return Ok(StackAction::Skip {
            reason: "native stacks require at least 2 PRs".to_string(),
        });
    }
    if !platform.supports_native_stacks() {
        return Ok(StackAction::Skip {
            reason: "platform does not support native stacks".to_string(),
        });
    }

    // Find stacks containing any of our PRs
    let mut found: Option<crate::types::PrStack> = None;
    for pr in prs {
        if let Some(stack) = platform.find_stack_for_pr(pr.number).await? {
            match &found {
                None => found = Some(stack),
                Some(existing) if existing.number == stack.number => {}
                Some(existing) => {
                    return Ok(StackAction::ReorderConflict {
                        message: format!(
                            "PRs span multiple stacks (#{} and #{}); run `ryu unstack` and resubmit",
                            existing.number, stack.number
                        ),
                    });
                }
            }
        }
    }

    let ours: Vec<u64> = prs.iter().map(|p| p.number).collect();

    let Some(stack) = found else {
        // Creating requires each PR's base to be the previous PR's head (422 otherwise)
        for pair in prs.windows(2) {
            if pair[1].base_ref != pair[0].head_ref {
                return Ok(StackAction::ReorderConflict {
                    message: format!(
                        "PR #{} base ({}) does not match PR #{} head ({}); update PR bases first",
                        pair[1].number, pair[1].base_ref, pair[0].number, pair[0].head_ref
                    ),
                });
            }
        }
        return Ok(StackAction::Create { prs: ours });
    };

    // Compare against the stack's open PRs, ordered bottom to top
    let remote: Vec<u64> = stack
        .pull_requests
        .iter()
        .filter(|e| e.merged_at.is_none() && e.state == "open")
        .map(|e| e.number)
        .collect();

    if remote == ours {
        return Ok(StackAction::NoOp {
            stack_number: stack.number,
        });
    }

    // Submitting a bottom slice of a taller registered stack (e.g. --upto)
    if remote.len() > ours.len() && remote[..ours.len()] == ours[..] {
        return Ok(StackAction::NoOp {
            stack_number: stack.number,
        });
    }

    if ours.len() > remote.len() && ours[..remote.len()] == remote[..] {
        return Ok(StackAction::Add {
            stack_number: stack.number,
            delta: ours[remote.len()..].to_vec(),
        });
    }

    Ok(StackAction::ReorderConflict {
        message: format!(
            "local stack does not match stack #{} (remote: {remote:?}, local: {ours:?}); run `ryu unstack` and resubmit to recreate",
            stack.number
        ),
    })
}

/// Format a stack action for dry-run output
pub fn format_stack_action(action: &StackAction) -> String {
    match action {
        StackAction::Create { prs } => {
            let list = prs
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(" → ");
            format!("create native stack: {list}")
        }
        StackAction::Add {
            stack_number,
            delta,
        } => {
            let list = delta
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("add {list} to stack #{stack_number}")
        }
        StackAction::NoOp { stack_number } => {
            format!("stack #{stack_number} already up to date")
        }
        StackAction::Skip { reason } => format!("skip stack registration ({reason})"),
        StackAction::ReorderConflict { message } => format!("stack conflict: {message}"),
    }
}

/// Report the intended stack action for a dry run
///
/// PR numbers are only known for pre-existing PRs; when any bookmark has no
/// PR yet, fall back to bookmark names.
pub async fn report_stack_dry_run(
    platform: &dyn PlatformService,
    plan: &SubmissionPlan,
    progress: &dyn ProgressCallback,
) {
    let prs = ordered_prs(plan, &plan.existing_prs);

    if prs.len() == plan.segments.len() && !prs.is_empty() {
        match plan_stack_action(platform, &prs).await {
            Ok(action) => {
                progress
                    .on_message(&format!("  → {}", format_stack_action(&action)))
                    .await;
            }
            Err(Error::StacksUnavailable(_)) => {
                progress
                    .on_message("  → skip stack registration (stacks unavailable)")
                    .await;
            }
            Err(e) => {
                progress
                    .on_message(&format!("  → stack registration unknown (lookup failed: {e})"))
                    .await;
            }
        }
    } else if plan.segments.len() >= 2 {
        let names = plan
            .segments
            .iter()
            .map(|s| s.bookmark.name.as_str())
            .collect::<Vec<_>>()
            .join(" → ");
        progress
            .on_message(&format!("  → register native stack: {names}"))
            .await;
    }
}

/// Run the stack registration phase
///
/// Returns `Some(message)` on soft failure (to be recorded in
/// `SubmissionResult.errors`), `None` on success or intentional skip.
#[allow(clippy::implicit_hasher)]
pub async fn execute_stack_registration(
    platform: &dyn PlatformService,
    plan: &SubmissionPlan,
    bookmark_to_pr: &HashMap<String, PullRequest>,
    progress: &dyn ProgressCallback,
) -> Option<String> {
    let prs = ordered_prs(plan, bookmark_to_pr);
    if prs.is_empty() {
        return None;
    }

    let backoffs = RETRY_BACKOFF_MS.iter().copied().chain(std::iter::once(0));
    for backoff_ms in backoffs {
        let action = match plan_stack_action(platform, &prs).await {
            Ok(action) => action,
            Err(Error::StacksUnavailable(msg)) => {
                return Some(format!("Stack registration skipped: {msg}"));
            }
            Err(e) => {
                return Some(format!("Stack registration failed: {e}"));
            }
        };

        debug!(?action, "stack registration action");

        let result: crate::error::Result<String> = match &action {
            StackAction::Skip { reason } => {
                progress
                    .on_message(&format!("Stack registration skipped ({reason})"))
                    .await;
                return None;
            }
            StackAction::NoOp { stack_number } => {
                progress
                    .on_message(&format!("Stack #{stack_number} already up to date"))
                    .await;
                return None;
            }
            StackAction::ReorderConflict { message } => return Some(message.clone()),
            StackAction::Create { prs } => platform
                .create_stack(prs)
                .await
                .map(|s| format!("Registered stack #{} ({} PRs)", s.number, prs.len())),
            StackAction::Add {
                stack_number,
                delta,
            } => match platform.add_to_stack(*stack_number, delta).await {
                Ok(_) => Ok(format!(
                    "Added {} PR(s) to stack #{stack_number}",
                    delta.len()
                )),
                Err(Error::StackNotFound(_)) => {
                    // Stack dissolved between check and write: fall back to create
                    let all: Vec<u64> = prs.iter().map(|p| p.number).collect();
                    platform
                        .create_stack(&all)
                        .await
                        .map(|s| format!("Registered stack #{} ({} PRs)", s.number, all.len()))
                }
                Err(e) => Err(e),
            },
        };

        match result {
            Ok(message) => {
                progress.on_message(&message).await;
                return None;
            }
            Err(Error::StackConflict(msg)) if backoff_ms > 0 => {
                debug!(backoff_ms, "stack concurrently modified, retrying");
                progress
                    .on_message(&format!("Stack concurrently modified, retrying: {msg}"))
                    .await;
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            }
            Err(Error::StacksUnavailable(msg)) => {
                return Some(format!("Stack registration skipped: {msg}"));
            }
            Err(e) => {
                return Some(format!("Stack registration failed: {e}"));
            }
        }
    }

    Some("Stack registration failed: retry limit exceeded".to_string())
}
