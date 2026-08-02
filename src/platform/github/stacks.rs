//! GitHub Stacks API client (public preview)
//!
//! Uses raw reqwest rather than octocrab because these endpoints are
//! status-code sensitive: 404 means both "feature unavailable" and "stack
//! gone" depending on route, 409 requires retry, and merge-async returns
//! semantically different 200/202 responses.

use super::GitHubService;
use crate::error::{Error, Result};
use crate::types::{
    MergeAsyncOutcome, MergeAsyncState, MergeOptions, PrStack,
};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use tracing::debug;

/// Body for stack create/add endpoints
fn pr_list_body(pull_requests: &[u64]) -> serde_json::Value {
    json!({ "pull_requests": pull_requests })
}

/// Read the response body, or return the empty string when there is none
async fn body_text(response: reqwest::Response) -> String {
    response.text().await.unwrap_or_default()
}

/// Map a non-success stacks API response to a structured error
async fn stacks_error(response: reqwest::Response, context: &str) -> Error {
    let status = response.status();
    let body = body_text(response).await;
    match status {
        StatusCode::NOT_FOUND => Error::StackNotFound(format!("{context}: {body}")),
        StatusCode::CONFLICT => Error::StackConflict(format!("{context}: {body}")),
        _ => Error::GitHubApi(format!("{context} failed ({status}): {body}")),
    }
}

/// Response shape for the merge-async poll endpoint
#[derive(Deserialize)]
struct MergeAsyncStatusResponse {
    status: MergeAsyncState,
}

impl GitHubService {
    /// `GET /repos/{o}/{r}/stacks?pull_request=N` → stacks containing the PR
    ///
    /// A 404 here means the Stacks feature is unavailable for the repo
    /// (rollout/GHES), mapped to [`Error::StacksUnavailable`].
    pub(crate) async fn stacks_find_for_pr(&self, pr_number: u64) -> Result<Option<PrStack>> {
        debug!(pr_number, "finding stack for PR");
        let response = self
            .stacks_http
            .get(format!("{}/stacks", self.repo_route()))
            .query(&[("pull_request", pr_number)])
            .send()
            .await?;

        match response.status() {
            StatusCode::OK => {
                let stacks: Vec<PrStack> = response.json().await?;
                // Ordered newest-first; the first is the current stack
                Ok(stacks.into_iter().next())
            }
            StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => Err(Error::StacksUnavailable(
                "stacks endpoint not available for this repository".to_string(),
            )),
            _ => Err(stacks_error(response, "list stacks").await),
        }
    }

    /// POST /repos/{o}/{r}/stacks with PRs ordered bottom to top (min 2)
    pub(crate) async fn stacks_create(&self, pull_requests: &[u64]) -> Result<PrStack> {
        debug!(?pull_requests, "creating stack");
        let response = self
            .stacks_http
            .post(format!("{}/stacks", self.repo_route()))
            .json(&pr_list_body(pull_requests))
            .send()
            .await?;

        if response.status() == StatusCode::CREATED {
            Ok(response.json().await?)
        } else {
            Err(stacks_error(response, "create stack").await)
        }
    }

    /// POST /repos/{o}/{r}/stacks/{n}/add, appending PRs to the top
    pub(crate) async fn stacks_add(
        &self,
        stack_number: u64,
        pull_requests: &[u64],
    ) -> Result<PrStack> {
        debug!(stack_number, ?pull_requests, "adding PRs to stack");
        let response = self
            .stacks_http
            .post(format!(
                "{}/stacks/{stack_number}/add",
                self.repo_route()
            ))
            .json(&pr_list_body(pull_requests))
            .send()
            .await?;

        if response.status() == StatusCode::OK {
            Ok(response.json().await?)
        } else {
            Err(stacks_error(response, "add to stack").await)
        }
    }

    /// POST /repos/{o}/{r}/stacks/{n}/unstack → 200 updated stack / 204 dissolved
    pub(crate) async fn stacks_unstack(&self, stack_number: u64) -> Result<Option<PrStack>> {
        debug!(stack_number, "unstacking");
        let response = self
            .stacks_http
            .post(format!(
                "{}/stacks/{stack_number}/unstack",
                self.repo_route()
            ))
            .send()
            .await?;

        match response.status() {
            StatusCode::OK => Ok(Some(response.json().await?)),
            StatusCode::NO_CONTENT => Ok(None),
            _ => Err(stacks_error(response, "unstack").await),
        }
    }

    /// PUT /repos/{o}/{r}/pulls/{n}/merge-async → 200 already done / 202 accepted
    pub(crate) async fn stacks_merge_async(
        &self,
        pr_number: u64,
        options: &MergeOptions,
    ) -> Result<MergeAsyncOutcome> {
        debug!(pr_number, method = %options.merge_method, "requesting async merge");
        let response = self
            .stacks_http
            .put(format!(
                "{}/pulls/{pr_number}/merge-async",
                self.repo_route()
            ))
            .json(options)
            .send()
            .await?;

        match response.status() {
            StatusCode::OK => Ok(MergeAsyncOutcome::AlreadyMergedOrQueued),
            StatusCode::ACCEPTED => {
                let body: serde_json::Value = response.json().await?;
                let uuid = body
                    .get("uuid")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        Error::GitHubApi("merge-async response missing uuid".to_string())
                    })?;
                Ok(MergeAsyncOutcome::Accepted {
                    uuid: uuid.to_string(),
                })
            }
            StatusCode::CONFLICT => Err(Error::MergeInProgress(body_text(response).await)),
            _ => Err(stacks_error(response, "merge-async").await),
        }
    }

    /// GET /repos/{o}/{r}/pulls/{n}/merge-async/{uuid}
    pub(crate) async fn stacks_poll_merge_async(
        &self,
        pr_number: u64,
        uuid: &str,
    ) -> Result<MergeAsyncState> {
        debug!(pr_number, uuid, "polling async merge");
        let response = self
            .stacks_http
            .get(format!(
                "{}/pulls/{pr_number}/merge-async/{uuid}",
                self.repo_route()
            ))
            .send()
            .await?;

        if response.status() == StatusCode::OK {
            let body: MergeAsyncStatusResponse = response.json().await?;
            Ok(body.status)
        } else {
            Err(stacks_error(response, "poll merge-async").await)
        }
    }
}
