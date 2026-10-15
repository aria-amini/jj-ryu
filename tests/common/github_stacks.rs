use super::GitHubService;
use crate::error::Error;
use crate::platform::PlatformService;
use crate::types::{MergeAsyncOutcome, MergeAsyncState, MergeOptions};

#[tokio::test]
async fn stack_lookup_ignores_closed_stacks() {
    let mut server = mockito::Server::new_async().await;
    let mut service =
        GitHubService::new("test-token", "owner".into(), "repo".into(), None).unwrap();
    service.api_base = server.url();

    // A closed result can precede the current stack; all-closed and empty
    // responses should both yield no current membership.
    for (stacks, expected) in [
        (vec![(9, false), (7, true), (6, true)], Some(7)),
        (vec![(9, false), (7, false)], None),
        (vec![], None),
    ] {
        let body: Vec<_> = stacks
            .into_iter()
            .map(|(number, open)| {
                serde_json::json!({
                    "id": number, "number": number, "url": "stack-url",
                    "base": {"ref": "main"}, "open": open
                })
            })
            .collect();
        let request = server
            .mock("GET", "/repos/owner/repo/stacks")
            .match_query(mockito::Matcher::UrlEncoded(
                "pull_request".into(),
                "2".into(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::to_string(&body).unwrap())
            .create_async()
            .await;
        assert_eq!(
            service
                .find_stack_for_pr(2)
                .await
                .unwrap()
                .map(|s| s.number),
            expected
        );
        request.assert_async().await;
    }
    drop(server);
}

#[tokio::test]
async fn accepted_merge_reads_nested_uuid_and_polls_result() {
    let mut server = mockito::Server::new_async().await;
    let request = server
        .mock("PUT", "/repos/owner/repo/pulls/2/merge-async")
        .with_status(202)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"status":"pending","details":{"message":"Merge requested","uuid":"merge-id","merge_method":"squash","merge_action":"direct_merge","expected_head_sha":"head-sha"}}"#,
        )
        .create_async()
        .await;
    let poll = server
        .mock("GET", "/repos/owner/repo/pulls/2/merge-async/merge-id")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"status":"merged","details":{"message":"Merged","sha":"merge-sha"}}"#)
        .create_async()
        .await;
    let mut service =
        GitHubService::new("test-token", "owner".into(), "repo".into(), None).unwrap();
    service.api_base = server.url();

    let outcome = service
        .merge_pr_async(2, &MergeOptions::default())
        .await
        .unwrap();
    let MergeAsyncOutcome::Accepted { uuid } = outcome else {
        panic!("Expected an accepted merge");
    };
    assert_eq!(uuid, "merge-id");
    assert_eq!(
        service.poll_merge_async(2, &uuid).await.unwrap(),
        MergeAsyncState::Merged
    );
    request.assert_async().await;
    poll.assert_async().await;
    drop(server);
}

#[tokio::test]
async fn accepted_merge_without_uuid_explains_that_request_was_accepted() {
    let mut server = mockito::Server::new_async().await;
    let request = server
        .mock("PUT", "/repos/owner/repo/pulls/2/merge-async")
        .with_status(202)
        .with_header("content-type", "application/json")
        .with_body(r#"{"status":"pending","details":{"message":"Merge requested"}}"#)
        .create_async()
        .await;
    let mut service =
        GitHubService::new("test-token", "owner".into(), "repo".into(), None).unwrap();
    service.api_base = server.url();

    let error = service
        .merge_pr_async(2, &MergeOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::GitHubApi(_)));
    assert!(error.to_string().contains("request accepted"));
    assert!(error.to_string().contains("details.uuid"));
    request.assert_async().await;
    drop(server);
}

#[tokio::test]
async fn standalone_merge_uses_fetched_head_sha_and_selected_method() {
    let mut server = mockito::Server::new_async().await;
    let head = server
        .mock("GET", "/repos/owner/repo/pulls/7")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"head":{"sha":"reviewed-head"}}"#)
        .create_async()
        .await;
    let merge = server
        .mock("PUT", "/repos/owner/repo/pulls/7/merge-async")
        .match_body(mockito::Matcher::Json(serde_json::json!({
            "sha": "reviewed-head", "merge_method": "rebase"
        })))
        .with_status(202)
        .with_header("content-type", "application/json")
        .with_body(r#"{"status":"pending","details":{"uuid":"single-id"}}"#)
        .create_async()
        .await;
    let mut service =
        GitHubService::new("test-token", "owner".into(), "repo".into(), None).unwrap();
    service.api_base = server.url();
    let sha = service.pr_head_sha(7).await.unwrap();
    let outcome = service
        .merge_pr_async(
            7,
            &MergeOptions {
                sha: Some(sha),
                merge_method: crate::types::MergeMethod::Rebase,
                ..MergeOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        MergeAsyncOutcome::Accepted {
            uuid: "single-id".into()
        }
    );
    head.assert_async().await;
    merge.assert_async().await;
    drop(server);
}

#[tokio::test]
async fn head_lookup_requires_a_successful_response_with_a_sha() {
    let mut server = mockito::Server::new_async().await;
    let mut service =
        GitHubService::new("test-token", "owner".into(), "repo".into(), None).unwrap();
    service.api_base = server.url();
    for (status, body, expected) in [
        (200, r#"{"head":{}}"#, "missing head.sha"),
        (200, r#"{"head":{"sha":""}}"#, "missing head.sha"),
        (403, r#"{"message":"Forbidden"}"#, "403"),
    ] {
        let request = server
            .mock("GET", "/repos/owner/repo/pulls/7")
            .with_status(status)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create_async()
            .await;
        let error = service.pr_head_sha(7).await.unwrap_err();
        assert!(error.to_string().contains(expected));
        request.assert_async().await;
    }
    drop(server);
}
