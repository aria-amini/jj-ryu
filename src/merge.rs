//! Async merge orchestration for native GitHub stacks
//!
//! Stacked PRs must be merged via GitHub's async merge API: merging PR #k
//! merges every unmerged PR below it atomically, and GitHub retargets and
//! server-side rebases the remainder of the stack.

use crate::error::{Error, Result};
use crate::platform::PlatformService;
use crate::types::{MergeAsyncState, PrStack, PrStackEntry};
use std::time::{Duration, Instant};

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
