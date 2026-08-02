//! Mock platform service for testing
//!
//! These are test utilities - not all may be used in current tests but are
//! available for future test development.

#![allow(dead_code)]

use async_trait::async_trait;
use jj_ryu::error::{Error, Result};
use jj_ryu::platform::PlatformService;
use jj_ryu::types::{
    MergeAsyncOutcome, MergeAsyncState, MergeOptions, Platform, PlatformConfig, PrStack,
    PrStackEntry, PrStackHead, PullRequest, StackRef,
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Call record for `create_pr`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatePrCall {
    pub head: String,
    pub base: String,
    pub title: String,
}

/// Call record for `update_pr_base`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateBaseCall {
    pub pr_number: u64,
    pub new_base: String,
}

/// Call record for `add_to_stack`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddToStackCall {
    pub stack_number: u64,
    pub pull_requests: Vec<u64>,
}

/// Simple mock platform service for testing
///
/// This manually implements `PlatformService` rather than using mockall,
/// because mockall has issues with methods returning references.
///
/// Features:
/// - Auto-incrementing PR numbers
/// - Call tracking for verification
/// - Configurable responses per branch
/// - Error injection for failure path testing
pub struct MockPlatformService {
    config: PlatformConfig,
    next_pr_number: AtomicU64,
    next_stack_number: AtomicU64,
    find_pr_responses: Mutex<HashMap<String, Option<PullRequest>>>,
    find_stack_responses: Mutex<HashMap<u64, Option<PrStack>>>,
    // Call tracking
    find_pr_calls: Mutex<Vec<String>>,
    create_pr_calls: Mutex<Vec<CreatePrCall>>,
    update_base_calls: Mutex<Vec<UpdateBaseCall>>,
    find_stack_calls: Mutex<Vec<u64>>,
    create_stack_calls: Mutex<Vec<Vec<u64>>>,
    add_to_stack_calls: Mutex<Vec<AddToStackCall>>,
    unstack_calls: Mutex<Vec<u64>>,
    merge_calls: Mutex<Vec<u64>>,
    // Error injection
    error_on_find_pr: Mutex<Option<String>>,
    error_on_create_pr: Mutex<Option<String>>,
    error_on_update_base: Mutex<Option<String>>,
    error_on_find_stack: Mutex<Option<String>>,
    error_on_create_stack: Mutex<Option<String>>,
    error_on_add_to_stack: Mutex<Option<String>>,
    // Number of times `add_to_stack` returns StackConflict before succeeding
    conflict_add_to_stack: Mutex<usize>,
}

impl MockPlatformService {
    /// Create a new mock with the given config
    pub fn with_config(config: PlatformConfig) -> Self {
        Self {
            config,
            next_pr_number: AtomicU64::new(1),
            next_stack_number: AtomicU64::new(1),
            find_pr_responses: Mutex::new(HashMap::new()),
            find_stack_responses: Mutex::new(HashMap::new()),
            find_pr_calls: Mutex::new(Vec::new()),
            create_pr_calls: Mutex::new(Vec::new()),
            update_base_calls: Mutex::new(Vec::new()),
            find_stack_calls: Mutex::new(Vec::new()),
            create_stack_calls: Mutex::new(Vec::new()),
            add_to_stack_calls: Mutex::new(Vec::new()),
            unstack_calls: Mutex::new(Vec::new()),
            merge_calls: Mutex::new(Vec::new()),
            error_on_find_pr: Mutex::new(None),
            error_on_create_pr: Mutex::new(None),
            error_on_update_base: Mutex::new(None),
            error_on_find_stack: Mutex::new(None),
            error_on_create_stack: Mutex::new(None),
            error_on_add_to_stack: Mutex::new(None),
            conflict_add_to_stack: Mutex::new(0),
        }
    }

    // === Error injection methods ===

    /// Make `find_existing_pr` return an error
    pub fn fail_find_pr(&self, msg: &str) {
        *self.error_on_find_pr.lock().unwrap() = Some(msg.to_string());
    }

    /// Make `create_pr` return an error
    pub fn fail_create_pr(&self, msg: &str) {
        *self.error_on_create_pr.lock().unwrap() = Some(msg.to_string());
    }

    /// Make `update_pr_base` return an error
    pub fn fail_update_base(&self, msg: &str) {
        *self.error_on_update_base.lock().unwrap() = Some(msg.to_string());
    }

    /// Make `find_stack_for_pr` return an error
    pub fn fail_find_stack(&self, msg: &str) {
        *self.error_on_find_stack.lock().unwrap() = Some(msg.to_string());
    }

    /// Make `find_stack_for_pr` report stacks as unavailable
    pub fn stacks_unavailable(&self) {
        *self.error_on_find_stack.lock().unwrap() = Some("__unavailable__".to_string());
    }

    /// Make `create_stack` return an error
    pub fn fail_create_stack(&self, msg: &str) {
        *self.error_on_create_stack.lock().unwrap() = Some(msg.to_string());
    }

    /// Make `add_to_stack` return an error
    pub fn fail_add_to_stack(&self, msg: &str) {
        *self.error_on_add_to_stack.lock().unwrap() = Some(msg.to_string());
    }

    /// Make `add_to_stack` fail with `StackConflict` `times` times before succeeding
    pub fn conflict_add_to_stack(&self, times: usize) {
        *self.conflict_add_to_stack.lock().unwrap() = times;
    }

    /// Set the response for `find_existing_pr` for a specific branch
    pub fn set_find_pr_response(&self, branch: &str, pr: Option<PullRequest>) {
        self.find_pr_responses
            .lock()
            .unwrap()
            .insert(branch.to_string(), pr);
    }

    /// Set the stack returned by `find_stack_for_pr` for a specific PR
    pub fn set_stack_for_pr(&self, pr_number: u64, stack: Option<PrStack>) {
        self.find_stack_responses
            .lock()
            .unwrap()
            .insert(pr_number, stack);
    }

    // === Call verification methods ===

    /// Get all branches that `find_existing_pr` was called with
    pub fn get_find_pr_calls(&self) -> Vec<String> {
        self.find_pr_calls.lock().unwrap().clone()
    }

    /// Get all `create_pr` calls
    pub fn get_create_pr_calls(&self) -> Vec<CreatePrCall> {
        self.create_pr_calls.lock().unwrap().clone()
    }

    /// Get all `update_pr_base` calls
    pub fn get_update_base_calls(&self) -> Vec<UpdateBaseCall> {
        self.update_base_calls.lock().unwrap().clone()
    }

    /// Get all PR numbers `find_stack_for_pr` was called with
    pub fn get_find_stack_calls(&self) -> Vec<u64> {
        self.find_stack_calls.lock().unwrap().clone()
    }

    /// Get all `create_stack` calls
    pub fn get_create_stack_calls(&self) -> Vec<Vec<u64>> {
        self.create_stack_calls.lock().unwrap().clone()
    }

    /// Get all `add_to_stack` calls
    pub fn get_add_to_stack_calls(&self) -> Vec<AddToStackCall> {
        self.add_to_stack_calls.lock().unwrap().clone()
    }

    /// Get all `unstack` calls
    pub fn get_unstack_calls(&self) -> Vec<u64> {
        self.unstack_calls.lock().unwrap().clone()
    }

    /// Assert that `create_pr` was called with specific head and base
    pub fn assert_create_pr_called(&self, head: &str, base: &str) {
        let calls = self.get_create_pr_calls();
        assert!(
            calls.iter().any(|c| c.head == head && c.base == base),
            "Expected create_pr({head}, {base}) but got: {calls:?}"
        );
    }

    /// Assert that `update_pr_base` was called with specific args
    pub fn assert_update_base_called(&self, pr_number: u64, new_base: &str) {
        let calls = self.get_update_base_calls();
        assert!(
            calls
                .iter()
                .any(|c| c.pr_number == pr_number && c.new_base == new_base),
            "Expected update_pr_base({pr_number}, {new_base}) but got: {calls:?}"
        );
    }

    /// Assert that `find_existing_pr` was called for each bookmark
    pub fn assert_find_pr_called_for(&self, branches: &[&str]) {
        let calls = self.get_find_pr_calls();
        for branch in branches {
            assert!(
                calls.contains(&branch.to_string()),
                "Expected find_existing_pr({branch}) but got: {calls:?}"
            );
        }
    }
}

/// Build a mock stack from PR numbers (bottom to top)
pub fn make_stack(number: u64, pr_numbers: &[u64]) -> PrStack {
    PrStack {
        id: number * 1000,
        number,
        node_id: Some(format!("STACK_node_{number}")),
        url: format!("https://api.github.com/repos/test/repo/stacks/{number}"),
        base: StackRef {
            ref_field: "main".to_string(),
        },
        open: true,
        created_at: None,
        pull_requests: pr_numbers
            .iter()
            .map(|n| PrStackEntry {
                number: *n,
                state: "open".to_string(),
                draft: false,
                merged_at: None,
                head: PrStackHead {
                    ref_field: format!("feat-{n}"),
                    sha: format!("sha_{n}"),
                },
            })
            .collect(),
    }
}

#[async_trait]
impl PlatformService for MockPlatformService {
    async fn find_existing_pr(&self, head_branch: &str) -> Result<Option<PullRequest>> {
        self.find_pr_calls
            .lock()
            .unwrap()
            .push(head_branch.to_string());

        // Check for injected error
        if let Some(msg) = self.error_on_find_pr.lock().unwrap().as_ref() {
            return Err(Error::Platform(msg.clone()));
        }

        let responses = self.find_pr_responses.lock().unwrap();
        Ok(responses.get(head_branch).cloned().flatten())
    }

    async fn create_pr_with_options(
        &self,
        head: &str,
        base: &str,
        title: &str,
        draft: bool,
    ) -> Result<PullRequest> {
        self.create_pr_calls.lock().unwrap().push(CreatePrCall {
            head: head.to_string(),
            base: base.to_string(),
            title: title.to_string(),
        });

        // Check for injected error
        if let Some(msg) = self.error_on_create_pr.lock().unwrap().as_ref() {
            return Err(Error::Platform(msg.clone()));
        }

        let number = self.next_pr_number.fetch_add(1, Ordering::SeqCst);
        let pr = PullRequest {
            number,
            html_url: format!("https://github.com/test/repo/pull/{number}"),
            base_ref: base.to_string(),
            head_ref: head.to_string(),
            title: title.to_string(),
            node_id: Some(format!("PR_node_{number}")),
            is_draft: draft,
        };
        Ok(pr)
    }

    async fn update_pr_base(&self, pr_number: u64, new_base: &str) -> Result<PullRequest> {
        self.update_base_calls.lock().unwrap().push(UpdateBaseCall {
            pr_number,
            new_base: new_base.to_string(),
        });

        // Check for injected error
        if let Some(msg) = self.error_on_update_base.lock().unwrap().as_ref() {
            return Err(Error::Platform(msg.clone()));
        }

        Ok(PullRequest {
            number: pr_number,
            html_url: format!("https://github.com/test/repo/pull/{pr_number}"),
            base_ref: new_base.to_string(),
            head_ref: "updated".to_string(),
            title: "Updated PR".to_string(),
            node_id: Some(format!("PR_node_{pr_number}")),
            is_draft: false,
        })
    }

    async fn publish_pr(&self, pr_number: u64) -> Result<PullRequest> {
        Ok(PullRequest {
            number: pr_number,
            html_url: format!("https://github.com/test/repo/pull/{pr_number}"),
            base_ref: "main".to_string(),
            head_ref: "published".to_string(),
            title: "Published PR".to_string(),
            node_id: Some(format!("PR_node_{pr_number}")),
            is_draft: false, // After publishing, is_draft is false
        })
    }

    fn supports_native_stacks(&self) -> bool {
        self.config.platform == Platform::GitHub
    }

    async fn find_stack_for_pr(&self, pr_number: u64) -> Result<Option<PrStack>> {
        self.find_stack_calls.lock().unwrap().push(pr_number);

        if let Some(msg) = self.error_on_find_stack.lock().unwrap().as_ref() {
            if msg == "__unavailable__" {
                return Err(Error::StacksUnavailable(
                    "stacks endpoint not available".to_string(),
                ));
            }
            return Err(Error::Platform(msg.clone()));
        }

        let responses = self.find_stack_responses.lock().unwrap();
        Ok(responses.get(&pr_number).cloned().flatten())
    }

    async fn create_stack(&self, pull_requests: &[u64]) -> Result<PrStack> {
        self.create_stack_calls
            .lock()
            .unwrap()
            .push(pull_requests.to_vec());

        if let Some(msg) = self.error_on_create_stack.lock().unwrap().as_ref() {
            return Err(Error::Platform(msg.clone()));
        }

        let number = self.next_stack_number.fetch_add(1, Ordering::SeqCst);
        Ok(make_stack(number, pull_requests))
    }

    async fn add_to_stack(&self, stack_number: u64, pull_requests: &[u64]) -> Result<PrStack> {
        self.add_to_stack_calls
            .lock()
            .unwrap()
            .push(AddToStackCall {
                stack_number,
                pull_requests: pull_requests.to_vec(),
            });

        if let Some(msg) = self.error_on_add_to_stack.lock().unwrap().as_ref() {
            return Err(Error::Platform(msg.clone()));
        }

        let should_conflict = {
            let mut remaining = self.conflict_add_to_stack.lock().unwrap();
            if *remaining > 0 {
                *remaining -= 1;
                true
            } else {
                false
            }
        };
        if should_conflict {
            return Err(Error::StackConflict(
                "stack being modified by another request".to_string(),
            ));
        }

        Ok(make_stack(stack_number, pull_requests))
    }

    async fn unstack(&self, stack_number: u64) -> Result<Option<PrStack>> {
        self.unstack_calls.lock().unwrap().push(stack_number);
        Ok(None)
    }

    async fn merge_pr_async(
        &self,
        pr_number: u64,
        _options: &MergeOptions,
    ) -> Result<MergeAsyncOutcome> {
        self.merge_calls.lock().unwrap().push(pr_number);
        Ok(MergeAsyncOutcome::Accepted {
            uuid: "mock-uuid".to_string(),
        })
    }

    async fn poll_merge_async(
        &self,
        _pr_number: u64,
        _uuid: &str,
    ) -> Result<MergeAsyncState> {
        Ok(MergeAsyncState::Merged)
    }

    fn config(&self) -> &PlatformConfig {
        &self.config
    }
}
