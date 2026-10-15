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
