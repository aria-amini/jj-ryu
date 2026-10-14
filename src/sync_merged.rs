//! Merged-layer handling for sync
//!
//! After GitHub merges the bottom of a native stack it retargets the
//! remaining PRs and rebases their branches server-side. Locally, `ryu sync`
//! drops the merged layers (bookmark, tracking entry, PR cache entry) and
//! rebases the remaining stack onto trunk; the rewritten branches are then
//! reconciled by the normal submit machinery (lease-protected force push with
//! equivalent content).

use crate::error::Result;
use crate::platform::PlatformService;
use crate::repo::JjWorkspace;
use crate::tracking::{PrCache, TrackingState};
use std::collections::HashMap;

/// A tracked bookmark whose PR has merged
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedLayer {
    /// Bookmark name
    pub bookmark: String,
    /// Merged PR number, when known
    pub pr_number: Option<u64>,
}

/// Detect tracked bookmarks whose PRs have merged
///
/// Checks each cached PR on its own. A native stack is not enough: GitHub
/// dissolves a stack when fewer than two PRs remain, and chained PRs on other
/// platforms have no stack at all.
#[allow(clippy::implicit_hasher)]
pub async fn detect_merged_layers(
    platform: &dyn PlatformService,
    tracked: &[String],
    pr_numbers: &HashMap<String, u64>,
) -> Result<Vec<MergedLayer>> {
    let mut merged = Vec::new();
    for bookmark in tracked {
        let Some(&pr_number) = pr_numbers.get(bookmark) else {
            continue;
        };
        if platform.is_pr_merged(pr_number).await? {
            merged.push(MergedLayer {
                bookmark: bookmark.clone(),
                pr_number: Some(pr_number),
            });
        }
    }
    Ok(merged)
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
            // Merges land bottom-up, so every local commit below a merged tip
            // is on trunk. A rebase or merge-commit merge lands copies of all
            // of them, not only of the tip.
            let landed: Vec<String> = workspace
                .resolve_revset(&format!("::{tip} ~ ::trunk()"))?
                .into_iter()
                .map(|entry| entry.commit_id)
                .collect();
            workspace.abandon_commits(&landed)?;
        }
        tracking.untrack(&layer.bookmark);
        pr_cache.remove(&layer.bookmark);
    }
    workspace.rebase_stack_onto_trunk()
}
