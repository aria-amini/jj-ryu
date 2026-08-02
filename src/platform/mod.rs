//! Platform services for GitHub and GitLab
//!
//! Provides a unified interface for PR/MR operations across platforms.

mod detection;
mod factory;
mod github;
mod gitlab;

pub use detection::{detect_platform, parse_repo_info};
pub use factory::create_platform_service;
pub use github::GitHubService;
pub use gitlab::GitLabService;

use crate::error::{Error, Result};
use crate::types::{
    MergeAsyncOutcome, MergeAsyncState, MergeOptions, PlatformConfig, PrStack, PullRequest,
};
use async_trait::async_trait;

/// Platform service trait for PR/MR operations
///
/// This trait abstracts GitHub and GitLab operations, allowing the same
/// submission logic to work with either platform.
#[async_trait]
pub trait PlatformService: Send + Sync {
    /// Find an existing open PR for a head branch
    async fn find_existing_pr(&self, head_branch: &str) -> Result<Option<PullRequest>>;

    /// Create a new PR with default options (non-draft).
    ///
    /// This is a convenience method that delegates to [`create_pr_with_options`]
    /// with `draft: false`. Implementors should override `create_pr_with_options`,
    /// not this method.
    ///
    /// [`create_pr_with_options`]: Self::create_pr_with_options
    async fn create_pr(&self, head: &str, base: &str, title: &str) -> Result<PullRequest> {
        self.create_pr_with_options(head, base, title, false).await
    }

    /// Create a new PR with explicit draft option.
    ///
    /// Implementors must provide this method. The default [`create_pr`] method
    /// delegates here with `draft: false`.
    ///
    /// [`create_pr`]: Self::create_pr
    async fn create_pr_with_options(
        &self,
        head: &str,
        base: &str,
        title: &str,
        draft: bool,
    ) -> Result<PullRequest>;

    /// Update the base branch of an existing PR
    async fn update_pr_base(&self, pr_number: u64, new_base: &str) -> Result<PullRequest>;

    /// Publish a draft PR (convert to ready for review)
    async fn publish_pr(&self, pr_number: u64) -> Result<PullRequest>;

    /// Whether the platform supports native stacked PRs (GitHub Stacks API)
    fn supports_native_stacks(&self) -> bool {
        false
    }

    /// Find the native stack containing a PR (None if the PR is not stacked)
    async fn find_stack_for_pr(&self, pr_number: u64) -> Result<Option<PrStack>> {
        let _ = pr_number;
        Err(Error::Unsupported {
            feature: "native stacks",
            platform: self.config().platform,
        })
    }

    /// Create a native stack from PRs ordered bottom to top (min 2)
    async fn create_stack(&self, pull_requests: &[u64]) -> Result<PrStack> {
        let _ = pull_requests;
        Err(Error::Unsupported {
            feature: "native stacks",
            platform: self.config().platform,
        })
    }

    /// Append PRs to the top of an existing native stack
    async fn add_to_stack(&self, stack_number: u64, pull_requests: &[u64]) -> Result<PrStack> {
        let _ = (stack_number, pull_requests);
        Err(Error::Unsupported {
            feature: "native stacks",
            platform: self.config().platform,
        })
    }

    /// Remove all unmerged PRs from a native stack (None = stack dissolved)
    async fn unstack(&self, stack_number: u64) -> Result<Option<PrStack>> {
        let _ = stack_number;
        Err(Error::Unsupported {
            feature: "native stacks",
            platform: self.config().platform,
        })
    }

    /// Request an async merge of a stacked PR (merges it and all PRs below it)
    async fn merge_pr_async(
        &self,
        pr_number: u64,
        options: &MergeOptions,
    ) -> Result<MergeAsyncOutcome> {
        let _ = (pr_number, options);
        Err(Error::Unsupported {
            feature: "async merge",
            platform: self.config().platform,
        })
    }

    /// Poll the status of an async merge
    async fn poll_merge_async(&self, pr_number: u64, uuid: &str) -> Result<MergeAsyncState> {
        let _ = (pr_number, uuid);
        Err(Error::Unsupported {
            feature: "async merge",
            platform: self.config().platform,
        })
    }

    /// Get the platform configuration
    fn config(&self) -> &PlatformConfig;
}
