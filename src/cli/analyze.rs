//! Default analyze command - print stack visualization

use crate::cli::style::{self, Stylize, check, pipe, up_arrow};
use anstream::println;
use jj_ryu::error::Result;
use jj_ryu::graph::build_change_graph;
use jj_ryu::platform::{create_platform_service, parse_repo_info};
use jj_ryu::repo::{JjWorkspace, select_remote};
use jj_ryu::tracking::{load_pr_cache, load_tracking};
use std::collections::HashMap;
use std::path::Path;

/// Native stack membership info, keyed by branch name
struct StackAnnotation {
    number: u64,
    total: usize,
    /// Branch name -> (1-based position, merged)
    entries: HashMap<String, (usize, bool)>,
}

/// Resolve the platform config for the default remote (sync, best-effort)
fn stack_lookup_config(workspace: &JjWorkspace) -> Option<jj_ryu::types::PlatformConfig> {
    let remotes = workspace.git_remotes().ok()?;
    let remote_name = select_remote(&remotes, None).ok()?;
    let remote_info = remotes.iter().find(|r| r.name == remote_name)?;
    parse_repo_info(&remote_info.url).ok()
}

/// Look up native stack membership via a PR number.
///
/// Best-effort: any failure (unsupported platform, stacks unavailable, auth
/// problems) yields `None` and the view renders without stack info.
async fn native_stack_annotation(
    config: jj_ryu::types::PlatformConfig,
    pr_number: u64,
) -> Option<StackAnnotation> {
    let platform = create_platform_service(&config).await.ok()?;
    if !platform.supports_native_stacks() {
        return None;
    }

    let stack = platform.find_stack_for_pr(pr_number).await.ok()??;
    let entries = stack
        .pull_requests
        .iter()
        .enumerate()
        .map(|(i, e)| (e.head.ref_field.clone(), (i + 1, e.merged_at.is_some())))
        .collect();
    Some(StackAnnotation {
        number: stack.number,
        total: stack.pull_requests.len(),
        entries,
    })
}

/// Run the analyze command (default when no subcommand given)
///
/// Prints a text-based visualization of the current stack.
#[allow(clippy::too_many_lines)]
pub async fn run_analyze(path: &Path) -> Result<()> {
    // Open workspace
    let workspace = JjWorkspace::open(path)?;
    let workspace_root = workspace.workspace_root().to_path_buf();

    // Load tracking state and PR cache
    let tracking = load_tracking(&workspace_root).unwrap_or_default();
    let pr_cache = load_pr_cache(&workspace_root).unwrap_or_default();

    // Build change graph from working copy
    let graph = build_change_graph(&workspace)?;

    let Some(stack) = &graph.stack else {
        println!("{}", "No bookmark stack found".muted());
        println!();
        println!(
            "{}",
            "Stacks are bookmarks that point to commits between trunk and working copy.".muted()
        );
        println!(
            "{}",
            "Create a bookmark with: jj bookmark create <name>".muted()
        );
        return Ok(());
    };

    if stack.segments.is_empty() {
        println!("{}", "Stack has no segments".muted());
        return Ok(());
    }

    let annotation = match stack_lookup_config(&workspace) {
        Some(config) => {
            let bottom_pr = stack
                .segments
                .iter()
                .flat_map(|s| &s.bookmarks)
                .find_map(|b| pr_cache.get(&b.name))
                .map(|p| p.number);
            match bottom_pr {
                Some(pr_number) => native_stack_annotation(config, pr_number).await,
                None => None,
            }
        }
        None => None,
    };

    // Print header
    let leaf = stack.segments.last().unwrap();
    let leaf_name = &leaf.bookmarks[0].name;
    println!("{} {}", "Stack:".emphasis(), leaf_name.accent());
    println!();

    // Print each segment in reverse order (newest/leaf first, oldest last)
    for segment in stack.segments.iter().rev() {
        let bookmark_names: Vec<&str> = segment.bookmarks.iter().map(|b| b.name.as_str()).collect();

        // Print commits in segment (already newest-first from revset)
        for (j, change) in segment.changes.iter().enumerate() {
            let is_first_in_segment = j == 0;
            let commit_short = &change.commit_id[..8.min(change.commit_id.len())];
            let change_short = &change.change_id[..8.min(change.change_id.len())];

            let desc = if change.description_first_line.is_empty() {
                "(no description)"
            } else {
                &change.description_first_line
            };

            // Truncate description (char-safe for UTF-8)
            let max_desc = 50;
            let desc_display = if desc.chars().count() > max_desc {
                format!("{}...", desc.chars().take(max_desc - 3).collect::<String>())
            } else {
                desc.to_string()
            };

            let marker = if change.is_working_copy {
                style::CURRENT
            } else {
                style::BULLET
            };

            // Show bookmark on first commit of segment (the tip)
            if is_first_in_segment && !bookmark_names.is_empty() {
                for bm in &bookmark_names {
                    let bookmark = segment.bookmarks.iter().find(|b| b.name == *bm).unwrap();
                    let is_tracked = tracking.is_tracked(bm);

                    // Tracking/sync status indicator
                    let status = if is_tracked {
                        if bookmark.is_synced {
                            format!(" {}", check())
                        } else {
                            format!(" {}", up_arrow())
                        }
                    } else {
                        // Untracked: dimmed dot
                        format!(" {}", "·".muted())
                    };

                    // PR number from cache (tracked only)
                    let pr_info = if is_tracked {
                        pr_cache
                            .get(bm)
                            .map(|p| format!(" #{}", p.number))
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };

                    // Native stack membership (position/total, or merged)
                    let stack_info = annotation
                        .as_ref()
                        .and_then(|a| {
                            a.entries
                                .get(*bm)
                                .map(|(pos, merged)| (a.number, a.total, *pos, *merged))
                        })
                        .map(|(number, total, pos, merged)| {
                            if merged {
                                format!(" {}", "merged".success())
                            } else {
                                format!(" {}", format!("stack #{number} {pos}/{total}").muted())
                            }
                        })
                        .unwrap_or_default();

                    // Dim untracked bookmark names
                    if is_tracked {
                        println!(
                            "       [{}{}]{}{}",
                            bm.accent(),
                            pr_info.muted(),
                            stack_info,
                            status
                        );
                    } else {
                        println!("       [{}]{}", bm.muted(), status);
                    }
                }
            }
            println!(
                "    {}  {} {} {}",
                marker,
                change_short.muted(),
                commit_short.muted(),
                desc_display
            );
            println!("    {}", pipe());
        }
    }

    // Print trunk base at bottom
    println!("  {}", "trunk()".muted());
    println!();

    // Summary - count tracked vs total
    let total_bookmarks = stack.segments.iter().flat_map(|s| &s.bookmarks).count();
    let tracked_count = stack
        .segments
        .iter()
        .flat_map(|s| &s.bookmarks)
        .filter(|b| tracking.is_tracked(&b.name))
        .count();
    let untracked_count = total_bookmarks - tracked_count;

    if tracked_count > 0 {
        println!(
            "{} bookmark{} ({} tracked)",
            total_bookmarks.accent(),
            if total_bookmarks == 1 { "" } else { "s" },
            tracked_count
        );
    } else {
        println!(
            "{} bookmark{}",
            total_bookmarks.accent(),
            if total_bookmarks == 1 { "" } else { "s" }
        );
    }

    if graph.excluded_bookmark_count > 0 {
        println!(
            "{}",
            format!(
                "({} bookmark{} excluded due to merge commits)",
                graph.excluded_bookmark_count,
                if graph.excluded_bookmark_count == 1 {
                    ""
                } else {
                    "s"
                }
            )
            .muted()
        );
    }

    println!();
    println!(
        "{}",
        format!(
            "Legend: {} = tracked synced, {} = tracked needs push, · = untracked, {} = working copy",
            style::CHECK,
            style::UP_ARROW,
            style::CURRENT
        )
        .muted()
    );

    // Hint about tracking if untracked bookmarks exist
    if untracked_count > 0 {
        println!();
        println!(
            "{}",
            "(use 'ryu track' to track untracked bookmarks)".muted()
        );
    }

    println!();
    println!("To submit this stack: {}", "ryu submit".accent());

    Ok(())
}
