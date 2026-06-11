//! Contract tests for the Linear `SubjectBackend` implementation.
//!
//! Each test stands up a mockito GraphQL server, points `LinearBackend` at it,
//! and exercises one trait method end-to-end. Fixtures live in
//! `tests/fixtures/` so the assertions stay focused on mapping logic.
//!
//! Most tests now also mock Linear's `team.states.nodes` query — the backend
//! lazily fetches the team's workflow states on the first call that needs
//! status translation, then caches the result.

use std::collections::HashMap;
use std::sync::Arc;
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

fn backend_with_overrides(url: &str, overrides: HashMap<String, SubjectStatus>) -> LinearBackend {
    let config = LinearConfig::new("test-token", url, Some("ENG".to_string()))
        .with_status_overrides(overrides);
    LinearBackend::new(config).expect("backend should build")
}

/// Discriminates GraphQL POST bodies by the operation name embedded in
/// the `query` string. mockito's `match_body` accepts a closure-based
/// matcher we can wire to inspect the request body.
fn matches_operation(operation: &'static str) -> Matcher {
    Matcher::Regex(format!(r#"query\s+{operation}\b"#))
}

fn matches_mutation(operation: &'static str) -> Matcher {
    Matcher::Regex(format!(r#"mutation\s+{operation}\b"#))
}

#[tokio::test]
async fn list_returns_mapped_subjects() {
    let mut server = Server::new_async().await;
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_default.json"))
        .create_async()
        .await;
    let _issues = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusListIssues"))
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
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_default.json"))
        .create_async()
        .await;
    let _issue = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusGetIssue"))
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
    assert_eq!(subject.native_status.as_deref(), Some("Todo"));
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

    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_default.json"))
        .create_async()
        .await;

    // Capture the request body so we can assert the GraphQL variables shape.
    let captured: std::sync::Arc<Mutex<Option<serde_json::Value>>> =
        std::sync::Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();

    let _m = server
        .mock("POST", "/")
        .match_body(matches_mutation("AnimusUpdateIssue"))
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
    // Regression: update must send `stateId` (UUID), NOT `stateName`.
    assert!(
        input.get("stateName").is_none(),
        "update must not emit stateName: {input}"
    );
    assert_eq!(
        input.get("stateId").and_then(|v| v.as_str()),
        Some("state-progress"),
        "update must emit stateId from the team's workflow"
    );
    assert_eq!(
        input.get("assigneeId").and_then(|v| v.as_str()),
        Some("bob@example.com")
    );
    assert!(
        input.get("labelIds").is_none(),
        "update must not emit a replace-all labelIds payload: {input}"
    );
    assert_eq!(
        input
            .get("addedLabelIds")
            .and_then(|v| v.as_array())
            .map(|v| v.len()),
        Some(1)
    );
    assert_eq!(
        input
            .get("removedLabelIds")
            .and_then(|v| v.as_array())
            .map(|v| v.len()),
        Some(1)
    );
}

#[tokio::test]
async fn update_clear_assignee_serializes_null() {
    let mut server = Server::new_async().await;
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_default.json"))
        .create_async()
        .await;
    let captured: std::sync::Arc<Mutex<Option<serde_json::Value>>> =
        std::sync::Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    let _m = server
        .mock("POST", "/")
        .match_body(matches_mutation("AnimusUpdateIssue"))
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
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_default.json"))
        .create_async()
        .await;
    let _m = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusListIssues"))
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
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_default.json"))
        .create_async()
        .await;
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
        .match_body(matches_operation("AnimusListIssues"))
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

#[tokio::test]
async fn list_filters_by_native_status_state_name() {
    let mut server = Server::new_async().await;
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_default.json"))
        .create_async()
        .await;

    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    let _m = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusListIssues"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |req| {
            let body = req.body().expect("mockito should expose request body");
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            *captured_clone.lock().unwrap() = Some(parsed);
            fixture("list_three_issues.json").into_bytes()
        })
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let filter = SubjectFilter {
        native_status: Some("In Review".to_string()),
        ..Default::default()
    };
    backend.list(filter).await.expect("list should succeed");

    let body = captured
        .lock()
        .unwrap()
        .clone()
        .expect("server saw a request body");
    assert_eq!(
        body.pointer("/variables/filter/state/name/eq")
            .and_then(|v| v.as_str()),
        Some("In Review"),
        "native_status must translate to a state.name.eq filter"
    );
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

// =====================================================================
// Status map: runtime discovery + override regression tests
// =====================================================================

/// On the first call that needs status translation, the backend must query
/// `team.states.nodes` and populate the in-process cache. Subsequent calls
/// must reuse the cached map without re-querying.
#[tokio::test]
async fn fetches_workflow_states_on_first_call_and_caches() {
    let mut server = Server::new_async().await;
    // Expect the workflow-states query EXACTLY ONCE. mockito's
    // `expect(1)` would fail the assertion if it's not called once.
    let team_states_mock = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .expect(1)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_default.json"))
        .create_async()
        .await;
    let _issue = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusGetIssue"))
        .expect(2)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("get_one_issue.json"))
        .create_async()
        .await;

    let backend = backend_against(&server.url());

    // First call — should drive the workflow-states fetch.
    backend
        .get(&SubjectId::new("linear:ENG-1"))
        .await
        .expect("first get");
    // Second call — must NOT re-fetch workflow states.
    backend
        .get(&SubjectId::new("linear:ENG-1"))
        .await
        .expect("second get");

    team_states_mock.assert_async().await;
}

/// `LINEAR_STATUS_MAP` overrides must beat the type-based auto-map.
#[tokio::test]
async fn applies_user_override_from_env() {
    let mut server = Server::new_async().await;
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_custom.json"))
        .create_async()
        .await;

    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    let _update = server
        .mock("POST", "/")
        .match_body(matches_mutation("AnimusUpdateIssue"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |req| {
            let body = req.body().expect("body");
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            *captured_clone.lock().unwrap() = Some(parsed);
            fixture("update_issue_ok.json").into_bytes()
        })
        .create_async()
        .await;

    // Override "Spec" to InProgress instead of the default Ready.
    let mut overrides = HashMap::new();
    overrides.insert("Spec".to_string(), SubjectStatus::InProgress);
    let backend = backend_with_overrides(&server.url(), overrides);

    let patch = SubjectPatch {
        status: Some(SubjectStatus::InProgress),
        ..Default::default()
    };
    backend
        .update(&SubjectId::new("linear:ENG-1"), patch)
        .await
        .expect("update ok");

    // With Spec overridden to InProgress AND it being position 1 (lower
    // than uuid-impl's position 2), the override-winning state should be
    // the reverse lookup target.
    let body = captured
        .lock()
        .unwrap()
        .clone()
        .expect("server captured the body");
    let state_id = body
        .pointer("/variables/input/stateId")
        .and_then(|v| v.as_str())
        .expect("stateId in input");
    assert_eq!(
        state_id, "uuid-spec",
        "override-winning Spec (position 1) must be picked over Implementation (position 2)"
    );
}

/// When multiple Linear states map to the same animus status, the reverse
/// lookup must pick the lowest-position state (Linear's default "first"
/// state for that category).
#[tokio::test]
async fn update_resolves_ambiguous_status_by_lowest_position() {
    let mut server = Server::new_async().await;
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        // team_states_ambiguous has TWO Ready candidates: Backlog (pos 5.0)
        // and Todo (pos 1.0). Todo must win.
        .with_body(fixture("team_states_ambiguous.json"))
        .create_async()
        .await;

    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    let _update = server
        .mock("POST", "/")
        .match_body(matches_mutation("AnimusUpdateIssue"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |req| {
            let body = req.body().expect("body");
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            *captured_clone.lock().unwrap() = Some(parsed);
            fixture("update_issue_ok.json").into_bytes()
        })
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let patch = SubjectPatch {
        status: Some(SubjectStatus::Ready),
        ..Default::default()
    };
    backend
        .update(&SubjectId::new("linear:ENG-1"), patch)
        .await
        .expect("update ok");

    let body = captured
        .lock()
        .unwrap()
        .clone()
        .expect("server captured the body");
    let state_id = body
        .pointer("/variables/input/stateId")
        .and_then(|v| v.as_str())
        .expect("stateId");
    assert_eq!(
        state_id, "uuid-todo",
        "lowest-position Ready candidate (Todo, pos 1.0) must win over Backlog (pos 5.0)"
    );
}

/// When the team's workflow has no state mapping to the requested animus
/// status, `update()` must return `InvalidRequest` with a message pointing
/// the user at `LINEAR_STATUS_MAP`.
#[tokio::test]
async fn update_errors_clearly_when_no_state_maps_to_target() {
    let mut server = Server::new_async().await;
    // Workflow with no `cancelled` state.
    let body = serde_json::json!({
        "data": {
            "team": {
                "states": {
                    "nodes": [
                        { "id": "uuid-todo", "name": "Todo", "type": "unstarted", "position": 1.0 },
                        { "id": "uuid-prog", "name": "In Progress", "type": "started", "position": 2.0 },
                        { "id": "uuid-done", "name": "Done", "type": "completed", "position": 3.0 }
                    ]
                }
            }
        }
    });
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body.to_string())
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let patch = SubjectPatch {
        status: Some(SubjectStatus::Cancelled),
        ..Default::default()
    };
    let err = backend
        .update(&SubjectId::new("linear:ENG-1"), patch)
        .await
        .expect_err("update must error when no state maps to Cancelled");
    match err {
        BackendError::InvalidRequest(msg) => {
            assert!(
                msg.contains("Cancelled"),
                "error should name the unmapped status: {msg}"
            );
            assert!(
                msg.contains("LINEAR_STATUS_MAP"),
                "error should mention the override env var: {msg}"
            );
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

/// Regression: the outgoing GraphQL mutation must send `stateId` (Linear's
/// canonical UUID) — never `stateName`.
#[tokio::test]
async fn update_sends_state_id_not_state_name() {
    let mut server = Server::new_async().await;
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_default.json"))
        .create_async()
        .await;

    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    let _update = server
        .mock("POST", "/")
        .match_body(matches_mutation("AnimusUpdateIssue"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |req| {
            let body = req.body().expect("body");
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            *captured_clone.lock().unwrap() = Some(parsed);
            fixture("update_issue_ok.json").into_bytes()
        })
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let patch = SubjectPatch {
        status: Some(SubjectStatus::Done),
        ..Default::default()
    };
    backend
        .update(&SubjectId::new("linear:ENG-1"), patch)
        .await
        .expect("update ok");

    let body = captured
        .lock()
        .unwrap()
        .clone()
        .expect("server captured the body");
    let input = body.pointer("/variables/input").expect("has input");
    assert!(
        input.get("stateName").is_none(),
        "must NOT emit stateName: {input}"
    );
    let state_id = input
        .get("stateId")
        .and_then(|v| v.as_str())
        .expect("must emit stateId");
    // team_states_default's Done is state-done.
    assert_eq!(state_id, "state-done");
}

// =====================================================================
// `patch.comment` semantics (regression for GitHub issue #2):
//
// `SubjectPatch.comment` is "Optional comment to post alongside the
// update" per `animus-subject-protocol`. Linear treats this as a real
// activity-log comment via the `commentCreate` GraphQL mutation — NOT
// as an overwrite of the issue body's `description` field.
// =====================================================================

/// When a patch carries a comment plus a real field change, `issueUpdate`
/// must NOT include `description` and `commentCreate` must be called with
/// the issue's UUID and the comment body.
#[tokio::test]
async fn update_with_comment_posts_via_comment_create_not_description() {
    let mut server = Server::new_async().await;
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_default.json"))
        .create_async()
        .await;

    let captured_update: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_update_clone = captured_update.clone();
    let _issue_update = server
        .mock("POST", "/")
        .match_body(matches_mutation("AnimusUpdateIssue"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |req| {
            let body = req.body().expect("body");
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            *captured_update_clone.lock().unwrap() = Some(parsed);
            fixture("update_issue_ok.json").into_bytes()
        })
        .create_async()
        .await;

    let captured_comment: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_comment_clone = captured_comment.clone();
    let comment_mock = server
        .mock("POST", "/")
        .match_body(matches_mutation("AnimusCreateComment"))
        .expect(1)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |req| {
            let body = req.body().expect("body");
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            *captured_comment_clone.lock().unwrap() = Some(parsed);
            fixture("comment_create_ok.json").into_bytes()
        })
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let patch = SubjectPatch {
        status: Some(SubjectStatus::InProgress),
        comment: Some("draft ready for review".to_string()),
        ..Default::default()
    };
    backend
        .update(&SubjectId::new("linear:ENG-1"), patch)
        .await
        .expect("update with comment should succeed");

    // 1. issueUpdate must NOT include `description` — that would overwrite
    //    the issue body with the comment text.
    let update_body = captured_update
        .lock()
        .unwrap()
        .clone()
        .expect("issueUpdate was called");
    let update_input = update_body
        .pointer("/variables/input")
        .expect("issueUpdate variables has input");
    assert!(
        update_input.get("description").is_none(),
        "patch.comment must NOT be written to issue.description: {update_input}"
    );

    // 2. commentCreate must be called with the issue UUID and body.
    comment_mock.assert_async().await;
    let comment_body = captured_comment
        .lock()
        .unwrap()
        .clone()
        .expect("commentCreate was called");
    let comment_input = comment_body
        .pointer("/variables/input")
        .expect("commentCreate variables has input");
    assert_eq!(
        comment_input.get("issueId").and_then(|v| v.as_str()),
        Some("11111111-1111-4111-8111-111111111111"),
        "comment must address the Linear issue UUID, not the identifier: {comment_input}"
    );
    assert_eq!(
        comment_input.get("body").and_then(|v| v.as_str()),
        Some("draft ready for review")
    );
}

/// When the patch carries ONLY a comment (no other field changes), the
/// backend must skip `issueUpdate` (Linear treats empty input as a no-op
/// but it's still a wasted round-trip), fetch the issue via `AnimusGetIssue`
/// to resolve its UUID, and post the comment via `commentCreate`.
#[tokio::test]
async fn update_with_only_comment_skips_issue_update_and_uses_get() {
    let mut server = Server::new_async().await;
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_default.json"))
        .create_async()
        .await;

    // issueUpdate must NOT be called when the patch has no field changes.
    let no_update = server
        .mock("POST", "/")
        .match_body(matches_mutation("AnimusUpdateIssue"))
        .expect(0)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("update_issue_ok.json"))
        .create_async()
        .await;

    let get_called = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusGetIssue"))
        .expect(1)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("get_one_issue.json"))
        .create_async()
        .await;

    let captured_comment: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let captured_comment_clone = captured_comment.clone();
    let comment_called = server
        .mock("POST", "/")
        .match_body(matches_mutation("AnimusCreateComment"))
        .expect(1)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |req| {
            let body = req.body().expect("body");
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            *captured_comment_clone.lock().unwrap() = Some(parsed);
            fixture("comment_create_ok.json").into_bytes()
        })
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let patch = SubjectPatch {
        comment: Some("workflow started".to_string()),
        ..Default::default()
    };
    backend
        .update(&SubjectId::new("linear:ENG-1"), patch)
        .await
        .expect("comment-only update should succeed");

    no_update.assert_async().await;
    get_called.assert_async().await;
    comment_called.assert_async().await;

    let comment_body = captured_comment
        .lock()
        .unwrap()
        .clone()
        .expect("commentCreate was called");
    let issue_id = comment_body
        .pointer("/variables/input/issueId")
        .and_then(|v| v.as_str())
        .expect("issueId");
    assert_eq!(issue_id, "11111111-1111-4111-8111-111111111111");
}

/// `Some("")` for `patch.comment` is treated as "no comment" — we don't
/// burn a `commentCreate` round trip to post an empty body.
#[tokio::test]
async fn update_with_empty_comment_skips_comment_create() {
    let mut server = Server::new_async().await;
    let _team_states = server
        .mock("POST", "/")
        .match_body(matches_operation("AnimusTeamStates"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("team_states_default.json"))
        .create_async()
        .await;

    let _update = server
        .mock("POST", "/")
        .match_body(matches_mutation("AnimusUpdateIssue"))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("update_issue_ok.json"))
        .create_async()
        .await;

    let no_comment = server
        .mock("POST", "/")
        .match_body(matches_mutation("AnimusCreateComment"))
        .expect(0)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(fixture("comment_create_ok.json"))
        .create_async()
        .await;

    let backend = backend_against(&server.url());
    let patch = SubjectPatch {
        status: Some(SubjectStatus::Done),
        comment: Some(String::new()),
        ..Default::default()
    };
    backend
        .update(&SubjectId::new("linear:ENG-1"), patch)
        .await
        .expect("update ok");

    no_comment.assert_async().await;
}
