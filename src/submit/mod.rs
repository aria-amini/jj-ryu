//! Three-phase submission engine
//!
//! Handles the workflow of submitting stacked bookmarks as PRs/MRs:
//! 1. Analysis - understand what needs to be submitted
//! 2. Planning - determine what PRs to create/update
//! 3. Execution - perform the actual operations

mod analysis;
mod execute;
mod plan;
mod progress;
mod stack_register;

pub use analysis::{
    SubmissionAnalysis, analyze_submission, create_narrowed_segments, generate_pr_title,
    get_base_branch, select_bookmark_for_segment,
};
pub use execute::{SubmissionResult, execute_submission};
pub use plan::{
    ExecutionConstraint, ExecutionStep, PrBaseUpdate, PrToCreate, SubmissionPlan,
    create_submission_plan,
};
pub use progress::{NoopProgress, Phase, ProgressCallback, PushStatus};
pub use stack_register::{
    StackAction, execute_stack_registration, format_stack_action, ordered_prs, plan_stack_action,
};
