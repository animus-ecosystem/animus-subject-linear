//! Contract tests for the Linear `SubjectBackend` implementation.
//!
//! Each test stands up a mockito GraphQL server, points `LinearBackend` at it,
//! and exercises one trait method end-to-end. Fixtures live in
//! `tests/fixtures/` so the assertions stay focused on mapping logic.

use std::sync::Mutex;

use animus_plugin_protocol::HealthStatus;
use animus_subject_linear::backend::LinearBackend;
use animus_subject_linear::config::LinearConfig;
use animus_subject_protocol::{
    BackendError, Subject, SubjectBackend, SubjectFilter, SubjectId, SubjectPatch, SubjectStatus,
};
use mockito::{Matcher, Server};

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn fixture(name: &str) -> String {
    let path = format!("{FIXTURE_DIR}/{name}");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing fixture {path}: {e}"))
}

fn backend_against(url: &str) -> LinearBackend {
    let config = LinearConfig::new("test-token", url, Some("ENG".to_string()));
    LinearBackend::new(config).expect("backend should build")
}

fn backend_without_token(url: &str) -> LinearBackend {
    let config = LinearConfig::without_token(url, Some("ENG".to_string()));
    LinearBackend::new(config).expect("token-less backend should still build")
}

#[tokio::test]
async fn list_returns_mapped_subjects() {
    let mut server = Server::new_async().await;
    let _m = server
        .mock("POST", "/")
        .match_header("authorization", "test-token")
        .match_header("content-type", "application/json")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("list_three_issues.json"))
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let filter = SubjectFilter {
        status: vec![SubjectStatus::Ready, SubjectStatus::InProgress],
        limit: Some(50),
        ..Default::default()
    };

    let page = backend.list(filter).await.expect("list should succeed");

    assert_eq!(page.subjects.len(), 3, "fixture has three issues");
    assert!(page.next_cursor.is_none(), "fixture says no more pages");

    let first = &page.subjects[0];
    assert_eq!(first.id.as_str(), "linear:ENG-1");
    assert_eq!(first.title, "Investigate flaky test");
    assert_eq!(first.status, SubjectStatus::Ready);
    assert_eq!(first.priority, Some(3));
    assert_eq!(first.assignee.as_deref(), Some("alice@example.com"));
    assert_eq!(
        first.labels,
        vec!["backend".to_string(), "testing".to_string()]
    );
    assert_eq!(
        first.url.as_deref(),
        Some("https://linear.app/launchapp/issue/ENG-1")
    );
    assert_eq!(
        first
            .custom
            .get("linear_state_type")
            .and_then(|v| v.as_str()),
        Some("unstarted")
    );

    let second = &page.subjects[1];
    assert_eq!(second.id.as_str(), "linear:ENG-2");
    assert_eq!(second.status, SubjectStatus::InProgress);
    assert_eq!(second.assignee, None);
    assert_eq!(
        second.parent.as_ref().map(|p| p.as_str().to_string()),
        Some("linear:ENG-100".to_string())
    );

    let third = &page.subjects[2];
    assert_eq!(third.status, SubjectStatus::Done);
    assert_eq!(third.children.len(), 2);
    assert_eq!(third.children[0].as_str(), "linear:ENG-4");
    assert!(
        third.description.is_none(),
        "empty description should map to None"
    );
}

#[tokio::test]
async fn get_returns_subject_by_id() {
    let mut server = Server::new_async().await;
    let _m = server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("get_one_issue.json"))
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let subject = backend
        .get(&SubjectId::new("linear:ENG-1"))
        .await
        .expect("get should succeed");

    assert_eq!(subject.id.as_str(), "linear:ENG-1");
    assert_eq!(subject.kind, "issue");
    assert_eq!(subject.title, "Investigate flaky test");
    assert_eq!(subject.status, SubjectStatus::Ready);
    assert_eq!(
        subject.description.as_deref(),
        Some("The webhook integration test fails ~10% of the time.")
    );
}

#[tokio::test]
async fn get_rejects_non_linear_id() {
    let server = Server::new_async().await;
    let backend = backend_against(&server.url());
    let err = backend
        .get(&SubjectId::new("github:owner/repo#1"))
        .await
        .expect_err("non-linear id must be rejected");

    match err {
        BackendError::InvalidRequest(msg) => {
            assert!(
                msg.contains("linear:"),
                "error should mention prefix: {msg}"
            );
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn update_translates_patch() {
    let mut server = Server::new_async().await;

    // Capture the request body so we can assert the GraphQL variables shape.
    let captured: std::sync::Arc<Mutex<Option<serde_json::Value>>> =
        std::sync::Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();

    let _m = server
        .mock("POST", "/")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "variables": {
                "id": "ENG-1",
                "input": {
                    "stateName": "In Progress",
                    "labelIds": { "add": ["wip"], "remove": ["needs-triage"] },
                    "assigneeId": "bob@example.com"
                }
            }
        })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |req| {
            let body = req.body().expect("mockito should expose request body");
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            *captured_clone.lock().unwrap() = Some(parsed);
            fixture("update_issue_ok.json").into_bytes()
        })
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let patch = SubjectPatch {
        status: Some(SubjectStatus::InProgress),
        assignee: Some(Some("bob@example.com".to_string())),
        labels_add: vec!["wip".to_string()],
        labels_remove: vec!["needs-triage".to_string()],
        ..Default::default()
    };

    let updated = backend
        .update(&SubjectId::new("linear:ENG-1"), patch)
        .await
        .expect("update should succeed");

    assert_eq!(updated.status, SubjectStatus::InProgress);
    assert_eq!(updated.assignee.as_deref(), Some("bob@example.com"));
    assert!(updated.labels.contains(&"wip".to_string()));

    let body = captured
        .lock()
        .unwrap()
        .clone()
        .expect("server saw a request body");
    let variables = body.get("variables").expect("body has variables");
    assert_eq!(variables.get("id").and_then(|v| v.as_str()), Some("ENG-1"));
    let input = variables.get("input").expect("variables has input");
    assert_eq!(
        input.get("stateName").and_then(|v| v.as_str()),
        Some("In Progress")
    );
    assert_eq!(
        input.get("assigneeId").and_then(|v| v.as_str()),
        Some("bob@example.com")
    );
    let label_ids = input.get("labelIds").expect("input has labelIds");
    assert_eq!(
        label_ids
            .get("add")
            .and_then(|v| v.as_array())
            .map(|v| v.len()),
        Some(1)
    );
    assert_eq!(
        label_ids
            .get("remove")
            .and_then(|v| v.as_array())
            .map(|v| v.len()),
        Some(1)
    );
}

#[tokio::test]
async fn update_clear_assignee_serializes_null() {
    let mut server = Server::new_async().await;
    let captured: std::sync::Arc<Mutex<Option<serde_json::Value>>> =
        std::sync::Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    let _m = server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |req| {
            let body = req.body().expect("mockito should expose request body");
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            *captured_clone.lock().unwrap() = Some(parsed);
            fixture("update_issue_ok.json").into_bytes()
        })
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let patch = SubjectPatch {
        assignee: Some(None),
        ..Default::default()
    };

    backend
        .update(&SubjectId::new("linear:ENG-1"), patch)
        .await
        .expect("update should succeed");

    let body = captured
        .lock()
        .unwrap()
        .clone()
        .expect("server saw a request body");
    let input = body.pointer("/variables/input").expect("has input");
    assert!(
        input
            .get("assigneeId")
            .map(|v| v.is_null())
            .unwrap_or(false),
        "clear assignee should send null: {input}"
    );
}

#[tokio::test]
async fn health_returns_healthy_on_200() {
    let mut server = Server::new_async().await;
    let _m = server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("viewer_ok.json"))
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let health = backend.health().await.expect("health call should succeed");
    assert_eq!(health.status, HealthStatus::Healthy);
    assert!(health.last_error.is_none());
}

#[tokio::test]
async fn health_returns_unhealthy_on_401() {
    let mut server = Server::new_async().await;
    let _m = server
        .mock("POST", "/")
        .with_status(401)
        .with_header("content-type", "application/json")
        .with_body(fixture("viewer_unauthorized.json"))
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let health = backend
        .health()
        .await
        .expect("health call should not panic");
    assert_eq!(health.status, HealthStatus::Unhealthy);
    let message = health.last_error.expect("unhealthy should carry a reason");
    assert!(
        message.to_ascii_lowercase().contains("auth"),
        "error should mention auth: {message}"
    );
}

#[tokio::test]
async fn schema_returns_expected_shape() {
    let server = Server::new_async().await;
    let backend = backend_against(&server.url());
    let schema = backend.schema();
    assert_eq!(schema.kinds, vec!["issue".to_string()]);
    assert!(schema.status_values.contains(&SubjectStatus::Ready));
    assert!(schema.status_values.contains(&SubjectStatus::Done));
    assert!(schema.supports_pagination);
    assert!(!schema.supports_watch, "v0.1.0 is polling-only");
    assert!(schema
        .native_status_values
        .iter()
        .any(|s| s == "In Progress"));
    assert!(schema.custom_fields.iter().any(|c| c.key == "priority"));
}

#[tokio::test]
async fn list_propagates_graphql_errors() {
    let mut server = Server::new_async().await;
    let _m = server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{ "errors": [{ "message": "Internal server error" }] }"#)
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let err = backend
        .list(SubjectFilter::default())
        .await
        .expect_err("graphql error should bubble up");
    let _msg: BackendError = err;
}

#[tokio::test]
async fn list_returns_next_cursor_when_paginating() {
    let mut server = Server::new_async().await;
    let body = serde_json::json!({
        "data": {
            "issues": {
                "pageInfo": { "hasNextPage": true, "endCursor": "cursor-page-2" },
                "nodes": []
            }
        }
    })
    .to_string();
    let _m = server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body)
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let page = backend
        .list(SubjectFilter::default())
        .await
        .expect("list ok");
    assert_eq!(page.next_cursor.as_deref(), Some("cursor-page-2"));
    let _placeholder: Option<Subject> = page.subjects.into_iter().next();
}

/// Regression for the `--manifest` bug: building a backend without a
/// `LINEAR_API_TOKEN` must succeed. The CLI runtime parses `--manifest`
/// before any backend method is invoked, so config + construction need to
/// be credential-free.
#[tokio::test]
async fn backend_builds_without_token() {
    let server = Server::new_async().await;
    let backend = backend_without_token(&server.url());
    // Schema is a static description - no credentials needed.
    let schema = backend.schema();
    assert_eq!(schema.kinds, vec!["issue".to_string()]);
}

/// `health()` must NOT hit the network when there's no token; it should
/// return `Unhealthy` with a clear reason so `animus plugin list` can still
/// surface the plugin in catalog output.
#[tokio::test]
async fn health_is_unhealthy_without_token() {
    let server = Server::new_async().await;
    let backend = backend_without_token(&server.url());
    let health = backend.health().await.expect("health should not error");
    assert_eq!(health.status, HealthStatus::Unhealthy);
    let msg = health.last_error.expect("missing-token should explain why");
    assert!(
        msg.contains("LINEAR_API_TOKEN"),
        "error should mention env var: {msg}"
    );
    assert!(msg.contains("unset"), "error should say `unset`: {msg}");
}

/// Authenticated methods must reject calls cleanly when the token is missing,
/// using `BackendError::Other` carrying "LINEAR_API_TOKEN required".
#[tokio::test]
async fn list_get_update_require_token() {
    let server = Server::new_async().await;
    let backend = backend_without_token(&server.url());

    let list_err = backend
        .list(SubjectFilter::default())
        .await
        .expect_err("list should error without token");
    match list_err {
        BackendError::Other(e) => assert!(
            e.to_string().contains("LINEAR_API_TOKEN required"),
            "list err: {e}"
        ),
        other => panic!("expected BackendError::Other, got {other:?}"),
    }

    let get_err = backend
        .get(&SubjectId::new("linear:ENG-1"))
        .await
        .expect_err("get should error without token");
    match get_err {
        BackendError::Other(e) => assert!(
            e.to_string().contains("LINEAR_API_TOKEN required"),
            "get err: {e}"
        ),
        other => panic!("expected BackendError::Other, got {other:?}"),
    }

    let update_err = backend
        .update(&SubjectId::new("linear:ENG-1"), SubjectPatch::default())
        .await
        .expect_err("update should error without token");
    match update_err {
        BackendError::Other(e) => assert!(
            e.to_string().contains("LINEAR_API_TOKEN required"),
            "update err: {e}"
        ),
        other => panic!("expected BackendError::Other, got {other:?}"),
    }
}
