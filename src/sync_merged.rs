//! Stale-bookmark cleanup for sync
//!
//! After GitHub merges the bottom of a native stack it retargets the
//! remaining PRs and rebases their branches server-side. Locally, `ryu sync`
//! drops the merged layers (bookmark, tracking entry, PR cache entry) and
//! rebases the remaining stack onto trunk; the rewritten branches are then
//! reconciled by the normal submit machinery (lease-protected force push with
//! equivalent content).
//!
//! Cleanup covers every local bookmark, tracked or not, in any workspace's
//! stack. [`plan_prune`] decides what to remove; [`apply_prune`] does it.

use crate::error::Result;
use crate::platform::PlatformService;
use crate::repo::JjWorkspace;
use crate::tracking::{PrCache, TrackingState};
use crate::types::PrState;
use std::collections::HashSet;

/// A local bookmark that sync will delete because its work landed in trunk
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedLayer {
    /// Bookmark name
    pub bookmark: String,
    /// Merged PR number, or `None` when the bookmark is already an ancestor
    /// of trunk
    pub pr_number: Option<u64>,
}

/// A local bookmark and the PR that explains why sync reports it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrBookmark {
    /// Bookmark name
    pub bookmark: String,
    /// PR number
    pub pr_number: u64,
}

/// What sync cleanup would do
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrunePlan {
    /// Bookmarks whose work landed in trunk: deleted, with their landed commits
    pub merged: Vec<MergedLayer>,
    /// Bookmarks whose PR closed without merging: deleted only with
    /// `--prune-closed`, and then without abandoning their commits
    pub closed: Vec<PrBookmark>,
    /// Bookmarks whose PR merged, but whose tip is not the merged head (the
    /// name was reused or the bookmark moved): never deleted
    pub diverged: Vec<PrBookmark>,
    /// Tracked bookmarks that no longer exist locally
    pub dangling: Vec<String>,
}

/// A local bookmark that may need cleanup
#[derive(Debug, Clone)]
struct Candidate {
    name: String,
    commit_id: String,
    /// Strict ancestor of trunk
    landed: bool,
}

/// What [`plan_prune`] needs from the repo, read before any platform call
#[derive(Debug, Clone)]
pub struct PruneInput {
    candidates: Vec<Candidate>,
    dangling: Vec<String>,
}

/// Read the local bookmarks that cleanup considers
///
/// Every local bookmark is a candidate, whether or not it is tracked or in
/// the current stack. The default branch never is. Reading is separate from
/// [`plan_prune`] so the repo is not held across platform calls.
pub fn collect_prune_input(
    workspace: &JjWorkspace,
    tracking: &TrackingState,
) -> Result<PruneInput> {
    let default_branch = workspace.default_branch()?;
    let trunk_tip = workspace
        .resolve_revset("trunk()")?
        .into_iter()
        .next()
        .map(|entry| entry.commit_id);
    let in_trunk: HashSet<String> = workspace
        .resolve_revset("bookmarks() & ::trunk()")?
        .into_iter()
        .map(|entry| entry.commit_id)
        .collect();

    let mut candidates: Vec<Candidate> = workspace
        .local_bookmarks()?
        .into_iter()
        .filter(|b| b.name != default_branch)
        .map(|b| Candidate {
            landed: in_trunk.contains(&b.commit_id) && trunk_tip.as_ref() != Some(&b.commit_id),
            name: b.name,
            commit_id: b.commit_id,
        })
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));

    // Tracked entries whose bookmark was deleted by hand. `bookmarks()` also
    // lists conflicted bookmarks, which `local_bookmarks` skips.
    let existing: HashSet<String> = workspace
        .resolve_revset("bookmarks()")?
        .into_iter()
        .flat_map(|entry| entry.local_bookmarks)
        .collect();
    let mut dangling: Vec<String> = tracking
        .tracked_names()
        .into_iter()
        .filter(|name| !existing.contains(*name))
        .map(str::to_string)
        .collect();
    dangling.sort();

    Ok(PruneInput {
        candidates,
        dangling,
    })
}

/// Decide which local bookmarks sync cleanup removes
///
/// - a strict ancestor of trunk has nothing unique left: `merged`
/// - otherwise (including a bookmark exactly at the trunk tip, which a fresh
///   task bookmark also is) the platform must show a PR from this branch
///   whose head commit is the bookmark's tip: merged PR -> `merged`, closed
///   PR -> `closed`
/// - a bookmark with no such PR evidence is never touched; a merged PR whose
///   head differs from the tip is reported as `diverged`
///
/// The PR is matched by head branch on the platform, not by the PR cache, so
/// untracked bookmarks work too.
pub async fn plan_prune(platform: &dyn PlatformService, input: PruneInput) -> Result<PrunePlan> {
    let mut plan = PrunePlan {
        dangling: input.dangling,
        ..PrunePlan::default()
    };
    for candidate in input.candidates {
        if candidate.landed {
            plan.merged.push(MergedLayer {
                bookmark: candidate.name,
                pr_number: None,
            });
            continue;
        }

        let prs = platform.find_prs_for_branch(&candidate.name).await?;
        if prs.iter().any(|pr| pr.state == PrState::Open) {
            continue;
        }
        let at_tip = |state| {
            prs.iter()
                .find(|pr| pr.state == state && pr.head_sha == candidate.commit_id)
        };
        if let Some(pr) = at_tip(PrState::Merged) {
            plan.merged.push(MergedLayer {
                bookmark: candidate.name,
                pr_number: Some(pr.number),
            });
        } else if let Some(pr) = at_tip(PrState::Closed) {
            plan.closed.push(PrBookmark {
                bookmark: candidate.name,
                pr_number: pr.number,
            });
        } else if let Some(pr) = prs.iter().find(|pr| pr.state == PrState::Merged) {
            plan.diverged.push(PrBookmark {
                bookmark: candidate.name,
                pr_number: pr.number,
            });
        }
    }
    Ok(plan)
}

/// Apply a [`PrunePlan`]; closed-PR bookmarks go only when `prune_closed`
///
/// Returns the number of root commits reparented onto trunk. The current
/// stack is rebased only when a dropped layer was part of it.
pub fn apply_prune(
    workspace: &mut JjWorkspace,
    tracking: &mut TrackingState,
    pr_cache: &mut PrCache,
    plan: &PrunePlan,
    prune_closed: bool,
) -> Result<usize> {
    let reparented = if !plan.merged.is_empty()
        && drop_merged_layers(workspace, tracking, pr_cache, &plan.merged)?
    {
        workspace.rebase_stack_onto_trunk()?
    } else {
        0
    };
    // After the merged layers: a closed bookmark still protects its
    // ancestors while they are dropped.
    if prune_closed {
        for closed in &plan.closed {
            workspace.delete_local_bookmark(&closed.bookmark)?;
            tracking.untrack(&closed.bookmark);
            pr_cache.remove(&closed.bookmark);
        }
    }
    for name in &plan.dangling {
        tracking.untrack(name);
        pr_cache.remove(name);
    }
    Ok(reparented)
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
    drop_merged_layers(workspace, tracking, pr_cache, merged)?;
    workspace.rebase_stack_onto_trunk()
}

/// [`apply_merged_layers`] without the rebase
///
/// Returns whether the current stack (`trunk()..@`) needs rebasing onto
/// trunk: a dropped layer of it had all its commits abandoned, and no dropped
/// layer of it kept commits for another workspace or bookmark. The rebase
/// would rewrite the commits that were just kept.
fn drop_merged_layers(
    workspace: &mut JjWorkspace,
    tracking: &mut TrackingState,
    pr_cache: &mut PrCache,
    merged: &[MergedLayer],
) -> Result<bool> {
    let merged_names: HashSet<&str> = merged.iter().map(|m| m.bookmark.as_str()).collect();
    let mut abandoned_in_stack = false;
    let mut kept_in_stack = false;
    for layer in merged {
        // Read the tip fresh each time: abandoning a lower layer rewrites
        // the commits (and bookmark targets) of the layers above it
        let tip = workspace
            .get_local_bookmark(&layer.bookmark)?
            .map(|b| b.commit_id);
        let in_current_stack = match &tip {
            Some(tip) => !workspace
                .resolve_revset(&format!("{tip} & (::@ ~ ::trunk())"))?
                .is_empty(),
            None => false,
        };
        workspace.delete_local_bookmark(&layer.bookmark)?;
        if let Some(tip) = tip {
            // A rebase or merge-commit merge lands copies of every commit
            // below the tip, not only of the tip. A chained PR can also merge
            // into an open base branch instead of trunk, so keep every commit
            // that a remaining bookmark at or outside this layer's tip needs.
            // Descendant layers can be rebased after dropping landed ancestors.
            // Other workspaces' working copies are kept, with their ancestors,
            // even when they sit on the same commit as this workspace.
            let open: Vec<String> = workspace
                .local_bookmarks()?
                .into_iter()
                .filter(|b| !merged_names.contains(b.name.as_str()))
                .map(|b| b.commit_id)
                .collect();
            let others = workspace.other_working_copy_commits()?;
            let mut keep = if others.is_empty() {
                String::from("none()")
            } else {
                format!("({})", others.join(" | "))
            };
            if !open.is_empty() {
                keep = format!("{keep} | (({}) ~ ({tip}:: ~ {tip}))", open.join(" | "));
            }
            let landed_revset = format!("::{tip} ~ ::trunk() ~ ::({keep})");
            let landed: Vec<String> = workspace
                .resolve_revset(&landed_revset)?
                .into_iter()
                .map(|entry| entry.commit_id)
                .collect();
            let unlanded = workspace
                .resolve_revset(&format!("::{tip} ~ ::trunk()"))?
                .len();
            if in_current_stack {
                if landed.len() == unlanded {
                    abandoned_in_stack = true;
                } else {
                    kept_in_stack = true;
                }
            }
            workspace.abandon_commits(&landed)?;
        }
        tracking.untrack(&layer.bookmark);
        pr_cache.remove(&layer.bookmark);
    }
    Ok(abandoned_in_stack && !kept_in_stack)
}
