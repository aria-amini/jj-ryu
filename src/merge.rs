//! Merge target selection and async merge orchestration for GitHub PRs
//!
//! Stacked PRs must be merged via GitHub's async merge API: merging PR #k
//! merges every unmerged PR below it atomically, and GitHub retargets and
//! server-side rebases the remainder of the stack.

use crate::error::{Error, Result};
use crate::platform::PlatformService;
use crate::tracking::PrCache;
use crate::types::{MergeAsyncState, PrStack, PrStackEntry};
use crate::unstack::find_submitted_stack;
use std::time::{Duration, Instant};

/// A standalone PR or a native stack merge through a selected PR.
#[derive(Debug)]
pub enum MergeTarget {
    /// A PR outside a native stack.
    Single(u64),
    /// A native stack with its affected layers.
    Stack(MergePlan),
}

/// Resolve an explicit bookmark, the bottom native stack layer, or a lone open PR.
///
/// Callers must scope the cache to the selected remote. Multiple standalone PRs
/// require an explicit bookmark; stale merged/closed entries are ignored.
pub async fn resolve_merge_target(
    platform: &dyn PlatformService,
    pr_cache: &PrCache,
    bookmark: Option<&str>,
) -> Result<MergeTarget> {
    if let Some(bookmark) = bookmark {
        let number = platform
            .find_existing_pr(bookmark)
            .await?
            .ok_or_else(|| {
                Error::InvalidArgument(format!("no open PR found for bookmark '{bookmark}'"))
            })?
            .number;
        return match platform.find_stack_for_pr(number).await? {
            Some(stack) => Ok(MergeTarget::Stack(plan_merge(stack, number)?)),
            None => Ok(MergeTarget::Single(number)),
        };
    }

    let numbers: Vec<_> = pr_cache.prs.iter().map(|pr| pr.number).collect();
    if let Some(stack) = find_submitted_stack(platform, &numbers).await? {
        let number = stack
            .pull_requests
            .iter()
            .find(|entry| entry.merged_at.is_none())
            .map(|entry| entry.number)
            .ok_or_else(|| {
                Error::InvalidArgument(format!(
                    "every PR in stack #{} is already merged; run `ryu sync`",
                    stack.number
                ))
            })?;
        return Ok(MergeTarget::Stack(plan_merge(stack, number)?));
    }

    let mut open = Vec::new();
    for cached in &pr_cache.prs {
        if let Some(pr) = platform.find_existing_pr(&cached.bookmark).await?
            && pr.number == cached.number
            && !open.contains(&pr.number)
        {
            open.push(pr.number);
        }
    }
    match open.as_slice() {
        [number] => Ok(MergeTarget::Single(*number)),
        [] => Err(Error::InvalidArgument(
            "no open submitted PRs found; run `ryu submit` or specify a bookmark".to_string(),
        )),
        _ => Err(Error::InvalidArgument(
            "multiple open PRs found outside a native stack; specify `ryu merge <bookmark>`"
                .to_string(),
        )),
    }
}

/// Which stack layers a merge through a given PR will affect
#[derive(Debug)]
pub struct MergePlan {
    /// The stack the PR belongs to
    pub stack: PrStack,
    /// Unmerged entries from the bottom through the target (these will merge)
    pub layers: Vec<PrStackEntry>,
}

impl MergePlan {
    /// Entries above the target (these will be retargeted and rebased by GitHub)
    pub fn remaining(&self) -> Vec<&PrStackEntry> {
        self.stack
            .pull_requests
            .iter()
            .filter(|e| e.merged_at.is_none())
            .filter(|e| !self.layers.iter().any(|l| l.number == e.number))
            .collect()
    }
}

/// Compute which layers merge when `pr_number` is merged
///
/// # Errors
/// Returns an error if the PR is not in the stack or is already merged.
pub fn plan_merge(stack: PrStack, pr_number: u64) -> Result<MergePlan> {
    let position = stack
        .pull_requests
        .iter()
        .position(|e| e.number == pr_number)
        .ok_or_else(|| {
            Error::Internal(format!(
                "PR #{pr_number} not found in stack #{}",
                stack.number
            ))
        })?;

    if stack.pull_requests[position].merged_at.is_some() {
        return Err(Error::InvalidArgument(format!(
            "PR #{pr_number} is already merged; run `ryu sync` to update your local stack"
        )));
    }

    let layers = stack.pull_requests[..=position]
        .iter()
        .filter(|e| e.merged_at.is_none())
        .cloned()
        .collect();

    Ok(MergePlan { stack, layers })
}

/// Poll an accepted async merge until it resolves or the timeout elapses
///
/// Returns the final state; `Pending`/`Enqueued` means the timeout elapsed
/// while the merge was still processing.
pub async fn poll_merge(
    platform: &dyn PlatformService,
    pr_number: u64,
    uuid: &str,
    interval: Duration,
    timeout: Duration,
) -> Result<MergeAsyncState> {
    let start = Instant::now();
    loop {
        let state = platform.poll_merge_async(pr_number, uuid).await?;
        match state {
            MergeAsyncState::Pending | MergeAsyncState::Enqueued if start.elapsed() < timeout => {
                tokio::time::sleep(interval).await;
            }
            _ => return Ok(state),
        }
    }
}
