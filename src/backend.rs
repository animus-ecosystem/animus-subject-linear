//! [`LinearBackend`] - the `SubjectBackend` implementation for Linear.
//!
//! Maps the normalized Animus subject schema onto Linear's `Issue` GraphQL
//! type. The id convention is `linear:<issueIdentifier>` (e.g.
//! `linear:ENG-123`) so the daemon can dispatch a backend solely from the id
//! prefix.

use std::collections::BTreeMap;

use animus_plugin_protocol::{HealthCheckResult, HealthStatus};
use animus_subject_protocol::{
    BackendError, ChangeKind, CustomFieldKind, CustomFieldSpec, EventStream, Subject,
    SubjectBackend, SubjectFilter, SubjectId, SubjectList, SubjectPatch, SubjectSchema,
    SubjectStatus,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::client::LinearClient;
use crate::config::LinearConfig;
use crate::status_map;

const ID_PREFIX: &str = "linear:";
const SUBJECT_KIND: &str = "issue";

/// Error message returned when an authenticated method is called without
/// `LINEAR_API_TOKEN` being set.
const MISSING_TOKEN_MSG: &str = "LINEAR_API_TOKEN required";

/// Last-error string surfaced by `health()` when the token is missing.
const HEALTH_MISSING_TOKEN: &str = "LINEAR_API_TOKEN unset";

fn missing_token_error() -> BackendError {
    BackendError::Other(anyhow::anyhow!(MISSING_TOKEN_MSG))
}

/// GraphQL fields we read on every issue. Centralized so `list`, `get`, and
/// `update` all return the same shape.
const ISSUE_FIELDS: &str = r#"
id
identifier
title
description
priority
url
createdAt
updatedAt
state { id name type }
assignee { id name email }
labels(first: 50) { nodes { name } }
parent { identifier }
children(first: 50) { nodes { identifier } }
"#;

/// Linear backend plugin state.
#[derive(Debug, Clone)]
pub struct LinearBackend {
    client: LinearClient,
}

impl LinearBackend {
    /// Build a backend from configuration. Validates the API token is
    /// header-safe (but does NOT perform a network call - that's `health()`).
    pub fn new(config: LinearConfig) -> anyhow::Result<Self> {
        let client = LinearClient::new(&config)?;
        Ok(Self { client })
    }

    /// Translate a `linear:ENG-123`-style [`SubjectId`] into the bare
    /// `"ENG-123"` Linear identifier.
    fn native_id(id: &SubjectId) -> Result<String, BackendError> {
        let raw = id.as_str();
        raw.strip_prefix(ID_PREFIX)
            .map(str::to_string)
            .ok_or_else(|| {
                BackendError::InvalidRequest(format!(
                    "subject id {raw:?} is not a linear id (expected `linear:<identifier>` prefix)"
                ))
            })
    }

    /// Build the `subject/list` GraphQL query. We always request a page of
    /// up to `limit` issues, optionally filtered by team and updated-since.
    fn list_query() -> String {
        format!(
            r#"
            query AnimusListIssues($filter: IssueFilter, $first: Int, $after: String) {{
              issues(filter: $filter, first: $first, after: $after, orderBy: updatedAt) {{
                pageInfo {{ hasNextPage endCursor }}
                nodes {{
                  {ISSUE_FIELDS}
                }}
              }}
            }}
            "#
        )
    }

    fn get_query() -> String {
        format!(
            r#"
            query AnimusGetIssue($id: String!) {{
              issue(id: $id) {{
                {ISSUE_FIELDS}
              }}
            }}
            "#
        )
    }

    fn update_mutation() -> String {
        format!(
            r#"
            mutation AnimusUpdateIssue($id: String!, $input: IssueUpdateInput!) {{
              issueUpdate(id: $id, input: $input) {{
                success
                issue {{
                  {ISSUE_FIELDS}
                }}
              }}
            }}
            "#
        )
    }

    /// Build the `IssueFilter` GraphQL variable from a [`SubjectFilter`].
    fn build_issue_filter(&self, filter: &SubjectFilter) -> Value {
        let mut graphql_filter = serde_json::Map::new();

        if let Some(team_id) = self.client.team_id() {
            graphql_filter.insert("team".to_string(), json!({ "id": { "eq": team_id } }));
        }

        if !filter.status.is_empty() {
            let native_names: Vec<String> = filter
                .status
                .iter()
                .map(|s| status_map::animus_to_linear(*s).to_string())
                .collect();
            graphql_filter.insert(
                "state".to_string(),
                json!({ "name": { "in": native_names } }),
            );
        }

        if !filter.assignee.is_empty() {
            graphql_filter.insert(
                "assignee".to_string(),
                json!({
                    "or": [
                        { "email": { "in": &filter.assignee } },
                        { "name": { "in": &filter.assignee } }
                    ]
                }),
            );
        }

        if !filter.labels_any.is_empty() || !filter.labels_all.is_empty() {
            let mut clauses = Vec::new();
            if !filter.labels_any.is_empty() {
                clauses.push(json!({ "name": { "in": &filter.labels_any } }));
            }
            for label in &filter.labels_all {
                clauses.push(json!({ "name": { "eq": label } }));
            }
            graphql_filter.insert("labels".to_string(), json!({ "and": clauses }));
        }

        if let Some(updated_since) = filter.updated_since {
            graphql_filter.insert(
                "updatedAt".to_string(),
                json!({ "gte": updated_since.to_rfc3339() }),
            );
        }

        Value::Object(graphql_filter)
    }

    /// Translate a GraphQL `Issue` node into a normalized [`Subject`].
    fn issue_to_subject(issue: &Value) -> Result<Subject, BackendError> {
        let identifier = issue
            .get("identifier")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BackendError::Other(anyhow::anyhow!(
                    "linear issue node is missing `identifier`: {issue}"
                ))
            })?;

        let title = issue
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let description = issue
            .get("description")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let state_name = issue
            .get("state")
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let status = status_map::linear_to_animus(state_name);

        let priority = issue
            .get("priority")
            .and_then(|v| v.as_u64())
            .and_then(|n| u8::try_from(n).ok());

        let assignee = issue
            .get("assignee")
            .and_then(|a| {
                a.get("email")
                    .and_then(|v| v.as_str())
                    .or_else(|| a.get("name").and_then(|v| v.as_str()))
            })
            .map(str::to_string);

        let labels: Vec<String> = issue
            .get("labels")
            .and_then(|l| l.get("nodes"))
            .and_then(|n| n.as_array())
            .map(|nodes| {
                nodes
                    .iter()
                    .filter_map(|node| {
                        node.get("name")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let parent = issue
            .get("parent")
            .and_then(|p| p.get("identifier"))
            .and_then(|v| v.as_str())
            .map(|id| SubjectId::new(format!("{ID_PREFIX}{id}")));

        let children: Vec<SubjectId> = issue
            .get("children")
            .and_then(|c| c.get("nodes"))
            .and_then(|n| n.as_array())
            .map(|nodes| {
                nodes
                    .iter()
                    .filter_map(|node| {
                        node.get("identifier")
                            .and_then(|v| v.as_str())
                            .map(|id| SubjectId::new(format!("{ID_PREFIX}{id}")))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let url = issue
            .get("url")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let created_at = parse_timestamp(issue.get("createdAt"))?;
        let updated_at = parse_timestamp(issue.get("updatedAt"))?;

        let mut custom = BTreeMap::new();
        if let Some(state_type) = issue
            .get("state")
            .and_then(|s| s.get("type"))
            .and_then(|v| v.as_str())
        {
            custom.insert(
                "linear_state_type".to_string(),
                Value::String(state_type.to_string()),
            );
        }
        if let Some(native_name) = issue
            .get("state")
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str())
        {
            custom.insert(
                "linear_state_name".to_string(),
                Value::String(native_name.to_string()),
            );
        }
        if let Some(linear_uuid) = issue.get("id").and_then(|v| v.as_str()) {
            custom.insert(
                "linear_uuid".to_string(),
                Value::String(linear_uuid.to_string()),
            );
        }

        Ok(Subject {
            id: SubjectId::new(format!("{ID_PREFIX}{identifier}")),
            kind: SUBJECT_KIND.to_string(),
            title,
            description,
            status,
            priority,
            assignee,
            labels,
            parent,
            children,
            url,
            created_at,
            updated_at,
            custom,
        })
    }

    /// Build the `IssueUpdateInput` for a [`SubjectPatch`].
    ///
    /// Note: applying `status` translates to a Linear `stateId`, which would
    /// require us to first look up the team's workflow states to find the
    /// matching id. For v0.1.0 we forward a `stateName` hint through the
    /// `customStateName` mutation extension - if no extension is available the
    /// daemon will degrade gracefully via the `BackendError::InvalidRequest`
    /// path in `apply_patch_unsupported_status_change`.
    fn build_update_input(patch: &SubjectPatch) -> Result<Value, BackendError> {
        let mut input = serde_json::Map::new();

        if let Some(Some(assignee)) = &patch.assignee {
            input.insert("assigneeId".to_string(), Value::String(assignee.clone()));
        } else if let Some(None) = &patch.assignee {
            input.insert("assigneeId".to_string(), Value::Null);
        }

        if !patch.labels_add.is_empty() || !patch.labels_remove.is_empty() {
            input.insert(
                "labelIds".to_string(),
                json!({
                    "add": patch.labels_add,
                    "remove": patch.labels_remove
                }),
            );
        }

        if let Some(comment) = &patch.comment {
            input.insert("description".to_string(), Value::String(comment.clone()));
        }

        for (key, value) in &patch.custom {
            input.insert(key.clone(), value.clone());
        }

        if let Some(status) = patch.status {
            input.insert(
                "stateName".to_string(),
                Value::String(status_map::animus_to_linear(status).to_string()),
            );
        }

        Ok(Value::Object(input))
    }
}

#[async_trait]
impl SubjectBackend for LinearBackend {
    async fn list(&self, filter: SubjectFilter) -> Result<SubjectList, BackendError> {
        if !self.client.has_token() {
            return Err(missing_token_error());
        }
        let issue_filter = self.build_issue_filter(&filter);
        let limit = filter.limit.unwrap_or(50).clamp(1, 100) as i64;
        let variables = json!({
            "filter": issue_filter,
            "first": limit,
            "after": filter.cursor,
        });

        let response = self
            .client
            .execute(&Self::list_query(), variables)
            .await
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;

        let data = response.into_data().map_err(map_graphql_err)?;

        let issues_root = data
            .get("issues")
            .ok_or_else(|| BackendError::Other(anyhow::anyhow!("missing `issues` in response")))?;

        let nodes = issues_root
            .get("nodes")
            .and_then(|n| n.as_array())
            .ok_or_else(|| BackendError::Other(anyhow::anyhow!("missing `issues.nodes` array")))?;

        let mut subjects = Vec::with_capacity(nodes.len());
        for node in nodes {
            subjects.push(Self::issue_to_subject(node)?);
        }

        let next_cursor = issues_root.get("pageInfo").and_then(|p| {
            let has_next = p
                .get("hasNextPage")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if has_next {
                p.get("endCursor")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            } else {
                None
            }
        });

        Ok(SubjectList {
            subjects,
            next_cursor,
            fetched_at: Utc::now(),
        })
    }

    async fn get(&self, id: &SubjectId) -> Result<Subject, BackendError> {
        let native = Self::native_id(id)?;
        if !self.client.has_token() {
            return Err(missing_token_error());
        }
        let variables = json!({ "id": native });

        let response = self
            .client
            .execute(&Self::get_query(), variables)
            .await
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;

        let data = response.into_data().map_err(map_graphql_err)?;
        let issue = data
            .get("issue")
            .filter(|v| !v.is_null())
            .ok_or_else(|| BackendError::NotFound(id.to_string()))?;

        Self::issue_to_subject(issue)
    }

    async fn update(&self, id: &SubjectId, patch: SubjectPatch) -> Result<Subject, BackendError> {
        let native = Self::native_id(id)?;
        if !self.client.has_token() {
            return Err(missing_token_error());
        }
        let input = Self::build_update_input(&patch)?;
        let variables = json!({ "id": native, "input": input });

        let response = self
            .client
            .execute(&Self::update_mutation(), variables)
            .await
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;

        let data = response.into_data().map_err(map_graphql_err)?;

        let payload = data.get("issueUpdate").ok_or_else(|| {
            BackendError::Other(anyhow::anyhow!("missing `issueUpdate` in response"))
        })?;

        let success = payload
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !success {
            return Err(BackendError::InvalidRequest(format!(
                "linear rejected update for {id}: {payload}"
            )));
        }

        let issue = payload
            .get("issue")
            .filter(|v| !v.is_null())
            .ok_or_else(|| BackendError::NotFound(id.to_string()))?;

        Self::issue_to_subject(issue)
    }

    async fn watch(&self) -> Option<EventStream> {
        // v0.1.0: polling-only. v0.2 will subscribe to Linear's GraphQL
        // subscription endpoint (or webhooks) and emit `SubjectChangedEvent`s.
        let _change_kinds_used: &[ChangeKind] = &[
            ChangeKind::Created,
            ChangeKind::Updated,
            ChangeKind::StatusChanged,
            ChangeKind::Deleted,
        ];
        None
    }

    fn schema(&self) -> SubjectSchema {
        SubjectSchema {
            kinds: vec![SUBJECT_KIND.to_string()],
            status_values: vec![
                SubjectStatus::Ready,
                SubjectStatus::InProgress,
                SubjectStatus::Blocked,
                SubjectStatus::Done,
                SubjectStatus::Cancelled,
            ],
            supports_watch: false,
            supports_create: false,
            supports_pagination: true,
            native_status_values: status_map::KNOWN_NATIVE_STATUSES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            custom_fields: vec![
                CustomFieldSpec {
                    key: "priority".to_string(),
                    kind: CustomFieldKind::Number,
                    values: None,
                },
                CustomFieldSpec {
                    key: "linear_state_name".to_string(),
                    kind: CustomFieldKind::String,
                    values: None,
                },
                CustomFieldSpec {
                    key: "linear_state_type".to_string(),
                    kind: CustomFieldKind::Enum,
                    values: Some(vec![
                        "backlog".to_string(),
                        "unstarted".to_string(),
                        "started".to_string(),
                        "completed".to_string(),
                        "canceled".to_string(),
                        "triage".to_string(),
                    ]),
                },
                CustomFieldSpec {
                    key: "linear_uuid".to_string(),
                    kind: CustomFieldKind::String,
                    values: None,
                },
            ],
        }
    }

    async fn health(&self) -> Result<HealthCheckResult, BackendError> {
        if !self.client.has_token() {
            return Ok(HealthCheckResult {
                status: HealthStatus::Unhealthy,
                uptime_ms: None,
                memory_usage_bytes: None,
                last_error: Some(HEALTH_MISSING_TOKEN.to_string()),
            });
        }

        let response = self
            .client
            .execute("query AnimusHealth { viewer { id name } }", json!({}))
            .await
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;

        if response.errors.is_empty() && response.status.is_success() {
            Ok(HealthCheckResult {
                status: HealthStatus::Healthy,
                uptime_ms: None,
                memory_usage_bytes: None,
                last_error: None,
            })
        } else {
            let message = if response.errors.is_empty() {
                format!("linear returned HTTP {}", response.status)
            } else {
                response
                    .errors
                    .iter()
                    .map(|e| e.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            Ok(HealthCheckResult {
                status: HealthStatus::Unhealthy,
                uptime_ms: None,
                memory_usage_bytes: None,
                last_error: Some(message),
            })
        }
    }
}

fn parse_timestamp(value: Option<&Value>) -> Result<DateTime<Utc>, BackendError> {
    let raw = value
        .and_then(|v| v.as_str())
        .ok_or_else(|| BackendError::Other(anyhow::anyhow!("missing timestamp")))?;
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| BackendError::Other(anyhow::anyhow!("invalid timestamp {raw:?}: {e}")))
}

fn map_graphql_err(error: anyhow::Error) -> BackendError {
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    if lower.contains("authentication") || lower.contains("unauthorized") {
        BackendError::PermissionDenied(message)
    } else if lower.contains("not found") || lower.contains("entity not found") {
        BackendError::NotFound(message)
    } else {
        BackendError::Unavailable(message)
    }
}
