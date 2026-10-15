//! Native stack removal (GitHub Stacks API)

use crate::error::Result;
use crate::platform::PlatformService;
use crate::types::PrStack;

/// Outcome of removing unmerged PRs from a native stack
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnstackOutcome {
    /// Unmerged PRs removed; merged or queued PRs remain in the stack
    Remaining {
        /// Stack number
        stack_number: u64,
        /// Number of PRs remaining
        remaining: usize,
    },
    /// No PRs remain; the stack was dissolved
    Dissolved {
        /// Stack number
        stack_number: u64,
    },
}

/// Find the native stack containing any of the given PRs
///
/// PRs are checked in order; the first stack found is returned. Returns
/// `Ok(None)` when none of the PRs belongs to a stack.
pub async fn find_submitted_stack(
    platform: &dyn PlatformService,
    pr_numbers: &[u64],
) -> Result<Option<PrStack>> {
    for pr in pr_numbers {
        if let Some(stack) = platform.find_stack_for_pr(*pr).await? {
            return Ok(Some(stack));
        }
    }
    Ok(None)
}

/// Remove all unmerged PRs from a native stack
pub async fn unstack(platform: &dyn PlatformService, stack_number: u64) -> Result<UnstackOutcome> {
    match platform.unstack(stack_number).await? {
        Some(stack) => Ok(UnstackOutcome::Remaining {
            stack_number: stack.number,
            remaining: stack.pull_requests.len(),
        }),
        None => Ok(UnstackOutcome::Dissolved { stack_number }),
    }
}
