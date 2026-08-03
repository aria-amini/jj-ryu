//! Merged-layer handling for sync (native GitHub stacks)
//!
//! After GitHub merges the bottom of a stack it retargets the remaining PRs
//! and rebases their branches server-side. Locally, `ryu sync` drops the
//! merged layers (bookmark, tracking entry, PR cache entry) and rebases the
//! remaining stack onto trunk; the rewritten branches are then reconciled by
//! the normal submit machinery (lease-protected force push with equivalent
//! content).

use crate::error::{Error, Result};
use crate::platform::PlatformService;
use crate::repo::JjWorkspace;
use crate::tracking::{PrCache, TrackingState};
use std::collections::{HashMap, HashSet};

/// A tracked bookmark whose PR has merged
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedLayer {
    /// Bookmark name
    pub bookmark: String,
    /// Merged PR number, when known
    pub pr_number: Option<u64>,
}

/// Detect tracked bookmarks whose PRs have merged, via the native stack
///
/// Returns an empty vec when the platform doesn't support native stacks, no
/// tracked bookmark has a cached PR, the PRs aren't stacked, or the Stacks
/// API is unavailable for the repo.
#[allow(clippy::implicit_hasher)]
pub async fn detect_merged_layers(
    platform: &dyn PlatformService,
    tracked: &[String],
    pr_numbers: &HashMap<String, u64>,
) -> Result<Vec<MergedLayer>> {
    if !platform.supports_native_stacks() || tracked.is_empty() {
        return Ok(Vec::new());
    }

    let Some(lookup_pr) = tracked.iter().find_map(|b| pr_numbers.get(b)) else {
        return Ok(Vec::new());
    };

    let stack = match platform.find_stack_for_pr(*lookup_pr).await {
        Ok(Some(stack)) => stack,
        Ok(None) | Err(Error::StacksUnavailable(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let merged_refs: HashSet<&str> = stack
        .pull_requests
        .iter()
        .filter(|e| e.merged_at.is_some())
        .map(|e| e.head.ref_field.as_str())
        .collect();

    Ok(tracked
        .iter()
        .filter(|b| merged_refs.contains(b.as_str()))
        .map(|b| MergedLayer {
            bookmark: b.clone(),
            pr_number: pr_numbers.get(b).copied(),
        })
        .collect())
}

/// Drop merged layers: delete local bookmarks, abandon the merged commits
/// (their changes landed in trunk via the merge), untrack, uncache, and
/// rebase the remaining stack onto trunk
///
/// Returns the number of root commits reparented onto trunk.
pub fn apply_merged_layers(
    workspace: &mut JjWorkspace,
    tracking: &mut TrackingState,
    pr_cache: &mut PrCache,
    merged: &[MergedLayer],
) -> Result<usize> {
    for layer in merged {
        // Read the tip fresh each time: abandoning a lower layer rewrites
        // the commits (and bookmark targets) of the layers above it
        let tip = workspace
            .get_local_bookmark(&layer.bookmark)?
            .map(|b| b.commit_id);
        workspace.delete_local_bookmark(&layer.bookmark)?;
        if let Some(tip) = tip {
            workspace.abandon_commits(&[tip])?;
        }
        tracking.untrack(&layer.bookmark);
        pr_cache.remove(&layer.bookmark);
    }
    workspace.rebase_stack_onto_trunk()
}
