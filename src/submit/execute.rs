//! Phase 3: Submission execution
//!
//! Executes the submission plan: push, create PRs, update bases, register stack.

use crate::error::{Error, Result};
use crate::platform::PlatformService;
use crate::repo::JjWorkspace;
use crate::submit::plan::{PrBaseUpdate, PrToCreate};
use crate::submit::stack_register;
use crate::submit::{ExecutionStep, Phase, ProgressCallback, PushStatus, SubmissionPlan};
use crate::types::{Bookmark, PullRequest};
use std::collections::HashMap;

/// Result of submission execution
#[derive(Debug, Clone, Default)]
pub struct SubmissionResult {
    /// Whether execution succeeded
    pub success: bool,
    /// PRs that were created
    pub created_prs: Vec<PullRequest>,
    /// PRs that were updated (base changed)
    pub updated_prs: Vec<PullRequest>,
    /// Bookmarks that were pushed
    pub pushed_bookmarks: Vec<String>,
    /// Errors encountered (non-fatal)
    pub errors: Vec<String>,
}

impl SubmissionResult {
    /// Create a new successful result
    pub fn new() -> Self {
        Self {
            success: true,
            ..Default::default()
        }
    }

    /// Record a fatal error and mark as failed
    pub fn fail(&mut self, error: String) {
        self.errors.push(error);
        self.success = false;
    }

    /// Record a non-fatal error (soft fail)
    pub fn soft_fail(&mut self, error: String) {
        self.errors.push(error);
    }
}

/// Outcome of executing a single step
#[derive(Debug)]
pub enum StepOutcome {
    /// Step succeeded, optionally with a PR to track
    Success(Option<(String, PullRequest)>),
    /// Step failed fatally - stop execution
    FatalError(String),
    /// Step failed but execution should continue (soft fail)
    SoftError(String),
}

// =============================================================================
// Step Execution Functions (testable in isolation)
// =============================================================================

/// Execute a push step
pub fn execute_push(workspace: &mut JjWorkspace, bookmark: &Bookmark, remote: &str) -> StepOutcome {
    match workspace.git_push(&bookmark.name, remote) {
        Ok(()) => StepOutcome::Success(None),
        Err(e) => StepOutcome::FatalError(format!("Failed to push {}: {e}", bookmark.name)),
    }
}

/// Execute an update base step
pub async fn execute_update_base(
    platform: &dyn PlatformService,
    update: &PrBaseUpdate,
) -> StepOutcome {
    // GitHub rejects base edits while a PR belongs to a native stack, so
    // dissolve the stack first; registration at the end re-creates it
    if platform.supports_native_stacks() {
        match platform.find_stack_for_pr(update.pr.number).await {
            Ok(Some(stack)) => {
                if let Err(e) = platform.unstack(stack.number).await {
                    return StepOutcome::FatalError(format!(
                        "Failed to dissolve stack #{} before updating base for {}: {e}",
                        stack.number, update.bookmark.name
                    ));
                }
            }
            Ok(None) | Err(Error::StacksUnavailable(_)) => {}
            Err(e) => {
                return StepOutcome::FatalError(format!(
                    "Failed to check stack membership for {}: {e}",
                    update.bookmark.name
                ));
            }
        }
    }

    match platform
        .update_pr_base(update.pr.number, &update.expected_base)
        .await
    {
        Ok(updated_pr) => StepOutcome::Success(Some((update.bookmark.name.clone(), updated_pr))),
        Err(e) => StepOutcome::FatalError(format!(
            "Failed to update PR base for {}: {e}",
            update.bookmark.name
        )),
    }
}

/// Execute a create PR step
pub async fn execute_create_pr(platform: &dyn PlatformService, create: &PrToCreate) -> StepOutcome {
    match platform
        .create_pr_with_options(
            &create.bookmark.name,
            &create.base_branch,
            &create.title,
            create.draft,
        )
        .await
    {
        Ok(pr) => StepOutcome::Success(Some((create.bookmark.name.clone(), pr))),
        Err(e) => StepOutcome::FatalError(format!(
            "Failed to create PR for {}: {e}",
            create.bookmark.name
        )),
    }
}

/// Execute a publish PR step (soft fail on error)
pub async fn execute_publish_pr(platform: &dyn PlatformService, pr: &PullRequest) -> StepOutcome {
    match platform.publish_pr(pr.number).await {
        Ok(updated_pr) => StepOutcome::Success(Some((pr.head_ref.clone(), updated_pr))),
        Err(e) => StepOutcome::SoftError(format!("Failed to publish PR #{}: {e}", pr.number)),
    }
}

// =============================================================================
// Main Execution Orchestrator
// =============================================================================

/// Execute a submission plan
///
/// This performs the actual operations:
/// 1. Push bookmarks to remote
/// 2. Update PR bases
/// 3. Create new PRs
/// 4. Publish draft PRs
/// 5. Register PRs as a native GitHub stack
pub async fn execute_submission(
    plan: &SubmissionPlan,
    workspace: &mut JjWorkspace,
    platform: &dyn PlatformService,
    progress: &dyn ProgressCallback,
    dry_run: bool,
) -> Result<SubmissionResult> {
    let mut result = SubmissionResult::new();

    if dry_run {
        progress
            .on_message("Dry run - no changes will be made")
            .await;
        report_dry_run(plan, progress).await;
        stack_register::report_stack_dry_run(platform, plan, progress).await;
        return Ok(result);
    }

    // Track all PRs (existing + created) for stack registration
    let mut bookmark_to_pr: HashMap<String, PullRequest> = plan.existing_prs.clone();

    // Phase: Executing all steps
    progress.on_phase(Phase::Executing).await;

    for step in &plan.execution_steps {
        let outcome = execute_step(step, workspace, platform, &plan.remote, progress).await;

        match outcome {
            StepOutcome::Success(Some((bookmark, pr))) => {
                // Track the PR for stack registration
                match step {
                    ExecutionStep::CreatePr(_) => result.created_prs.push(pr.clone()),
                    ExecutionStep::UpdateBase(_) | ExecutionStep::PublishPr(_) => {
                        result.updated_prs.push(pr.clone());
                    }
                    ExecutionStep::Push(_) => {}
                }
                bookmark_to_pr.insert(bookmark, pr);
            }
            StepOutcome::Success(None) => {
                // Push succeeded - track it
                if let ExecutionStep::Push(bm) = step {
                    result.pushed_bookmarks.push(bm.name.clone());
                }
            }
            StepOutcome::FatalError(msg) => {
                progress.on_error(&Error::Platform(msg.clone())).await;
                result.fail(msg);
                return Ok(result);
            }
            StepOutcome::SoftError(msg) => {
                progress.on_error(&Error::Platform(msg.clone())).await;
                result.soft_fail(msg);
            }
        }
    }

    // Phase: Registering native GitHub stack (soft-fail)
    progress.on_phase(Phase::RegisteringStack).await;

    if let Some(msg) =
        stack_register::execute_stack_registration(platform, plan, &bookmark_to_pr, progress).await
    {
        progress.on_error(&Error::Platform(msg.clone())).await;
        result.soft_fail(msg);
    }

    progress.on_phase(Phase::Complete).await;

    Ok(result)
}

/// Execute a single step with progress reporting
async fn execute_step(
    step: &ExecutionStep,
    workspace: &mut JjWorkspace,
    platform: &dyn PlatformService,
    remote: &str,
    progress: &dyn ProgressCallback,
) -> StepOutcome {
    match step {
        ExecutionStep::Push(bookmark) => {
            progress
                .on_bookmark_push(&bookmark.name, PushStatus::Started)
                .await;

            let outcome = execute_push(workspace, bookmark, remote);

            match &outcome {
                StepOutcome::Success(_) => {
                    progress
                        .on_bookmark_push(&bookmark.name, PushStatus::Success)
                        .await;
                }
                StepOutcome::FatalError(msg) | StepOutcome::SoftError(msg) => {
                    progress
                        .on_bookmark_push(&bookmark.name, PushStatus::Failed(msg.clone()))
                        .await;
                }
            }

            outcome
        }

        ExecutionStep::UpdateBase(update) => {
            progress
                .on_message(&format!(
                    "Updating {} base: {} → {}",
                    update.bookmark.name, update.current_base, update.expected_base
                ))
                .await;

            let outcome = execute_update_base(platform, update).await;

            if let StepOutcome::Success(Some((bookmark, pr))) = &outcome {
                progress.on_pr_updated(bookmark, pr).await;
            }

            outcome
        }

        ExecutionStep::CreatePr(create) => {
            let draft_str = if create.draft { " [draft]" } else { "" };
            progress
                .on_message(&format!(
                    "Creating PR for {} (base: {}){draft_str}",
                    create.bookmark.name, create.base_branch
                ))
                .await;

            let outcome = execute_create_pr(platform, create).await;

            if let StepOutcome::Success(Some((bookmark, pr))) = &outcome {
                progress.on_pr_created(bookmark, pr).await;
            }

            outcome
        }

        ExecutionStep::PublishPr(pr) => {
            progress
                .on_message(&format!("Publishing PR #{} ({})", pr.number, pr.head_ref))
                .await;

            execute_publish_pr(platform, pr).await
        }
    }
}

// =============================================================================
// Dry Run Reporting
// =============================================================================

/// Report what would be done in a dry run
async fn report_dry_run(plan: &SubmissionPlan, progress: &dyn ProgressCallback) {
    if plan.execution_steps.is_empty() {
        progress.on_message("Nothing to do - already in sync").await;
        return;
    }

    progress.on_message("Would execute:").await;
    for step in &plan.execution_steps {
        let msg = format_step_for_dry_run(step, &plan.remote);
        progress.on_message(&msg).await;
    }
}

/// Format a step for dry run output
pub fn format_step_for_dry_run(step: &ExecutionStep, remote: &str) -> String {
    match step {
        // Push needs special handling to include remote
        ExecutionStep::Push(bm) => format!("  → push {} to {}", bm.name, remote),
        // All other steps use Display impl
        _ => format!("  → {step}"),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NarrowedBookmarkSegment;

    fn make_pr(number: u64, bookmark: &str) -> PullRequest {
        PullRequest {
            number,
            html_url: format!("https://github.com/test/test/pull/{number}"),
            base_ref: "main".to_string(),
            head_ref: bookmark.to_string(),
            title: format!("PR for {bookmark}"),
            node_id: Some(format!("PR_node_{number}")),
            is_draft: false,
        }
    }

    fn make_bookmark(name: &str) -> Bookmark {
        Bookmark {
            name: name.to_string(),
            commit_id: format!("{name}_commit"),
            change_id: format!("{name}_change"),
            has_remote: false,
            is_synced: false,
        }
    }

    // === SubmissionResult tests ===

    #[test]
    fn test_submission_result_new() {
        let result = SubmissionResult::new();
        assert!(result.success);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_submission_result_fail() {
        let mut result = SubmissionResult::new();
        result.fail("something went wrong".to_string());

        assert!(!result.success);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0], "something went wrong");
    }

    #[test]
    fn test_submission_result_soft_fail() {
        let mut result = SubmissionResult::new();
        result.soft_fail("minor issue".to_string());

        // Soft fail records error but doesn't mark as failed
        assert!(result.success);
        assert_eq!(result.errors.len(), 1);
    }

    // === StepOutcome tests ===

    #[test]
    fn test_step_outcome_success_without_pr() {
        let outcome = StepOutcome::Success(None);
        assert!(matches!(outcome, StepOutcome::Success(None)));
    }

    #[test]
    fn test_step_outcome_success_with_pr() {
        let pr = make_pr(1, "feat-a");
        let outcome = StepOutcome::Success(Some(("feat-a".to_string(), pr)));
        assert!(matches!(outcome, StepOutcome::Success(Some(_))));
    }

    #[test]
    fn test_step_outcome_fatal_error() {
        let outcome = StepOutcome::FatalError("boom".to_string());
        assert!(matches!(outcome, StepOutcome::FatalError(_)));
    }

    #[test]
    fn test_step_outcome_soft_error() {
        let outcome = StepOutcome::SoftError("minor".to_string());
        assert!(matches!(outcome, StepOutcome::SoftError(_)));
    }

    // === Dry run formatting tests ===

    #[test]
    fn test_format_step_push() {
        let bm = make_bookmark("feat-a");
        let step = ExecutionStep::Push(bm);
        let output = format_step_for_dry_run(&step, "origin");
        assert_eq!(output, "  → push feat-a to origin");
    }

    #[test]
    fn test_format_step_create_pr() {
        let bm = make_bookmark("feat-a");
        let create = PrToCreate {
            bookmark: bm,
            base_branch: "main".to_string(),
            title: "Add feature".to_string(),
            draft: false,
        };
        let step = ExecutionStep::CreatePr(create);
        let output = format_step_for_dry_run(&step, "origin");
        assert_eq!(output, "  → create PR feat-a → main (Add feature)");
    }

    #[test]
    fn test_format_step_create_pr_draft() {
        let bm = make_bookmark("feat-a");
        let create = PrToCreate {
            bookmark: bm,
            base_branch: "main".to_string(),
            title: "Add feature".to_string(),
            draft: true,
        };
        let step = ExecutionStep::CreatePr(create);
        let output = format_step_for_dry_run(&step, "origin");
        assert!(output.contains("[draft]"));
    }

    #[test]
    fn test_format_step_update_base() {
        let bm = make_bookmark("feat-b");
        let update = PrBaseUpdate {
            bookmark: bm,
            current_base: "main".to_string(),
            expected_base: "feat-a".to_string(),
            pr: make_pr(42, "feat-b"),
        };
        let step = ExecutionStep::UpdateBase(update);
        let output = format_step_for_dry_run(&step, "origin");
        assert_eq!(output, "  → update feat-b (PR #42) main → feat-a");
    }

    #[test]
    fn test_format_step_publish() {
        let pr = make_pr(99, "feat-a");
        let step = ExecutionStep::PublishPr(pr);
        let output = format_step_for_dry_run(&step, "origin");
        assert_eq!(output, "  → publish PR #99 (feat-a)");
    }

    // === Plan helper tests ===

    #[test]
    fn test_plan_is_empty() {
        let plan = SubmissionPlan {
            segments: vec![],
            constraints: vec![],
            execution_steps: vec![],
            existing_prs: HashMap::new(),
            remote: "origin".to_string(),
            default_branch: "main".to_string(),
        };

        assert!(plan.is_empty());
    }

    #[test]
    fn test_plan_counts() {
        let bm = make_bookmark("feat-a");
        let plan = SubmissionPlan {
            segments: vec![NarrowedBookmarkSegment {
                bookmark: bm.clone(),
                changes: vec![],
            }],
            constraints: vec![],
            execution_steps: vec![
                ExecutionStep::Push(bm.clone()),
                ExecutionStep::CreatePr(PrToCreate {
                    bookmark: bm,
                    base_branch: "main".to_string(),
                    title: "Add feat-a".to_string(),
                    draft: false,
                }),
            ],
            existing_prs: HashMap::new(),
            remote: "origin".to_string(),
            default_branch: "main".to_string(),
        };

        assert!(!plan.is_empty());
        assert_eq!(plan.count_pushes(), 1);
        assert_eq!(plan.count_creates(), 1);
        assert_eq!(plan.count_updates(), 0);
        assert_eq!(plan.count_publishes(), 0);
    }
}
