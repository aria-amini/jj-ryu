//! Shared CLI progress callback with styled output and spinners

use crate::cli::style::{Stream, Stylize, check, cross, hyperlink_url};
use anstream::{eprintln, print, println};
use async_trait::async_trait;
use jj_ryu::error::Error;
use jj_ryu::submit::{Phase, ProgressCallback, PushStatus};
use jj_ryu::types::PullRequest;
use std::io::Write;

/// CLI progress callback that prints to stdout with styled output
///
/// Two modes:
/// - verbose (submit): shows all phases, detailed messages
/// - compact (sync): inline status updates, indented for nested output
pub struct CliProgress {
    /// Verbose mode shows all phases and detailed output
    pub verbose: bool,
}

impl CliProgress {
    /// Create verbose progress (for submit command)
    pub const fn verbose() -> Self {
        Self { verbose: true }
    }

    /// Create compact progress (for sync command)
    pub const fn compact() -> Self {
        Self { verbose: false }
    }
}

#[async_trait]
impl ProgressCallback for CliProgress {
    async fn on_phase(&self, phase: Phase) {
        if self.verbose {
            println!("{}...", phase.to_string().emphasis());
        } else {
            match phase {
                Phase::Executing | Phase::RegisteringStack => {
                    println!("  {}...", phase.to_string().muted());
                }
                _ => {}
            }
        }
    }

    async fn on_bookmark_push(&self, bookmark: &str, status: PushStatus) {
        if self.verbose {
            match &status {
                PushStatus::Started => {
                    println!("  Pushing {}...", bookmark.accent());
                }
                PushStatus::Success => {
                    println!("  {} Pushed {}", check(), bookmark.emphasis());
                }
                PushStatus::AlreadySynced => {
                    println!(
                        "  {} {} {}",
                        "-".muted(),
                        bookmark.accent(),
                        status.to_string().muted()
                    );
                }
                PushStatus::Failed(_) => {
                    eprintln!(
                        "  {} Failed to push {}: {}",
                        cross(),
                        bookmark.accent().for_stderr(),
                        status.to_string().error()
                    );
                }
            }
        } else {
            match &status {
                PushStatus::Started => {
                    print!("    Pushing {}... ", bookmark.accent());
                    let _ = std::io::stdout().flush();
                }
                PushStatus::Success => {
                    println!("{}", "done".success());
                }
                _ => {
                    // Use warn style but on stdout for inline status
                    println!("{}", status.to_string().warn().for_stdout());
                }
            }
        }
    }

    async fn on_pr_created(&self, bookmark: &str, pr: &PullRequest) {
        let pr_num = format!("#{}", pr.number);
        if self.verbose {
            println!(
                "  {} Created PR {} for {}",
                check(),
                pr_num.accent(),
                bookmark.emphasis()
            );
            println!("    {}", hyperlink_url(Stream::Stdout, &pr.html_url));
        } else {
            println!(
                "    Created PR {} for {} ({})",
                pr_num.accent(),
                bookmark.accent(),
                hyperlink_url(Stream::Stdout, &pr.html_url)
            );
        }
    }

    async fn on_pr_updated(&self, bookmark: &str, pr: &PullRequest) {
        let pr_num = format!("#{}", pr.number);
        if self.verbose {
            println!(
                "  {} Updated PR {} for {}",
                check(),
                pr_num.accent(),
                bookmark.emphasis()
            );
        } else {
            println!(
                "    Updated PR {} for {}",
                pr_num.accent(),
                bookmark.accent()
            );
        }
    }

    async fn on_error(&self, err: &Error) {
        if self.verbose {
            eprintln!("{}: {}", "error".error(), err);
        } else {
            eprintln!("    {}: {}", "error".error(), err);
        }
    }

    async fn on_message(&self, message: &str) {
        if self.verbose {
            println!("{message}");
        } else {
            println!("  {}", message.muted());
        }
    }
}
