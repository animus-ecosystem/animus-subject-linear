//! [`LinearBackend`] - the `SubjectBackend` implementation for Linear.
//!
//! Maps the normalized Animus subject schema onto Linear's `Issue` GraphQL
//! type. The id convention is `linear:<issueIdentifier>` (e.g.
//! `linear:ENG-123`) so the daemon can dispatch a backend solely from the id
//! prefix.

use std::collections::BTreeMap;
use std::sync::Arc;

use animus_plugin_protocol::{HealthCheckResult, HealthStatus};
use animus_subject_protocol::{
    BackendError, ChangeKind, CustomFieldKind, CustomFieldSpec, EventStream, Subject,
    SubjectBackend, SubjectFilter, SubjectId, SubjectList, SubjectPatch, SubjectSchema,
    SubjectStatus,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::OnceCell;

use crate::client::LinearClient;
use crate::config::{parse_subject_status, LinearConfig};
use crate::status_map::{self, StatusMap};

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
    config: LinearConfig,
    /// Lazily populated on the first call that needs status translation.
    /// `Arc` so the backend stays `Clone` (the plugin host clones it across
    /// inbound dispatch calls) while sharing the single discovered map.
    status_map: Arc<OnceCell<StatusMap>>,
}

/// Flat request payload for `issue/create` / `subject/create`.
///
/// The daemon's `SubjectRouter` sends create params at the TOP LEVEL (unlike
/// `subject/update`, which wraps fields under `patch`). The CLI emits
/// `{title, body?, status?, priority?, labels?}`; richer callers (e.g.
/// `animus plugin call`) may also pass `project_id` / `team_id`.
#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    /// Issue title. Required; rejected when empty/whitespace.
    pub title: String,
    /// Markdown body. Maps to Linear's `description` (the wire key is `body`).
    #[serde(default)]
    pub body: Option<String>,
    /// Normalized lowercase status string (`ready`/`in-progress`/...).
    #[serde(default)]
    pub status: Option<String>,
    /// Priority bucket string (`p0`..`p3`), NOT a Linear integer.
    #[serde(default)]
    pub priority: Option<String>,
    /// Label names. Not yet honored on create in v0.1.8 (logged + dropped).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Optional project UUID; falls back to `LINEAR_PROJECT_ID`.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Optional team UUID/key; falls back to `LINEAR_TEAM_ID`.
    #[serde(default)]
    pub team_id: Option<String>,
}

impl LinearBackend {
    /// Build a backend from configuration. Validates the API token is
    /// header-safe (but does NOT perform a network call - that's `health()`).
    pub fn new(config: LinearConfig) -> anyhow::Result<Self> {
        let client = LinearClient::new(&config)?;
        Ok(Self {
            client,
            config,
            status_map: Arc::new(OnceCell::new()),
        })
    }

    /// Returns the cached [`StatusMap`], discovering it from Linear on the
    /// first call. Subsequent calls return the cached value without
    /// re-querying.
    ///
    /// Requires `team_id` to be configured (the workflow states query is
    /// scoped to a team).
    async fn status_map(&self) -> Result<&StatusMap, BackendError> {
        if !self.client.has_token() {
            return Err(missing_token_error());
        }
        let team_id = self.client.team_id().ok_or_else(|| {
            BackendError::InvalidRequest(
                "LINEAR_TEAM_ID must be set to discover workflow states for status mapping"
                    .to_string(),
            )
        })?;

        self.status_map
            .get_or_try_init(|| async {
                let states = self
                    .client
                    .fetch_workflow_states(team_id)
                    .await
                    .map_err(|e| BackendError::Unavailable(e.to_string()))?;
                Ok::<StatusMap, BackendError>(StatusMap::from_workflow_states(
                    &states,
                    &self.config.status_overrides,
                ))
            })
            .await
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

    fn create_mutation() -> String {
        format!(
            r#"
            mutation AnimusCreateIssue($input: IssueCreateInput!) {{
              issueCreate(input: $input) {{
                success
                issue {{
                  {ISSUE_FIELDS}
                }}
              }}
            }}
            "#
        )
    }

    /// Build the `commentCreate` mutation used when `SubjectPatch.comment` is set.
    ///
    /// Per the `SubjectPatch.comment` protocol contract ("Optional comment to
    /// post alongside the update"), Linear posts the text as a real issue
    /// comment via the `commentCreate` GraphQL mutation. We never write the
    /// comment text into the issue's `description` — that would silently
    /// destroy the issue body. The `issueId` argument is Linear's internal
    /// UUID (not the `ENG-123` identifier), captured from the `issueUpdate`
    /// (or `issue`) response.
    fn comment_mutation() -> &'static str {
        r#"
        mutation AnimusCreateComment($input: CommentCreateInput!) {
          commentCreate(input: $input) {
            success
            comment { id }
          }
        }
        "#
    }

    /// Build the `IssueFilter` GraphQL variable from a [`SubjectFilter`].
    fn build_issue_filter(&self, filter: &SubjectFilter, status_map: &StatusMap) -> Value {
        let mut graphql_filter = serde_json::Map::new();

        if let Some(team_id) = self.client.team_id() {
            graphql_filter.insert("team".to_string(), json!({ "id": { "eq": team_id } }));
        }

        let mut state_filter = serde_json::Map::new();
        if !filter.status.is_empty() {
            // Map each requested animus status to all Linear state UUIDs
            // that resolve to it. We filter by stateId rather than name
            // because names can collide across teams and stateId is the
            // canonical key.
            let state_ids: Vec<String> = status_map
                .iter()
                .filter(|(_, _, animus_status)| filter.status.contains(animus_status))
                .map(|(id, _, _)| id.to_string())
                .collect();
            if !state_ids.is_empty() {
                state_filter.insert("id".to_string(), json!({ "in": state_ids }));
            }
        }
        if let Some(native_status) = &filter.native_status {
            state_filter.insert("name".to_string(), json!({ "eq": native_status }));
        }
        if !state_filter.is_empty() {
            graphql_filter.insert("state".to_string(), Value::Object(state_filter));
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
    fn issue_to_subject(issue: &Value, status_map: &StatusMap) -> Result<Subject, BackendError> {
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

        // Prefer the state UUID for translation (stable, override-aware).
        // Fall back to the state name, then to the type-based default if
        // the team's workflow changed between discovery and this read.
        let state_id = issue
            .get("state")
            .and_then(|s| s.get("id"))
            .and_then(|v| v.as_str());
        let state_name = issue
            .get("state")
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let state_type = issue
            .get("state")
            .and_then(|s| s.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let status = state_id
            .and_then(|id| status_map.linear_to_animus(id))
            .or_else(|| status_map.linear_name_to_animus(state_name))
            .unwrap_or_else(|| status_map::type_to_animus(state_type));

        let priority = issue
            .get("priority")
            .and_then(|v| v.as_u64())
            .and_then(linear_priority_to_animus);

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
            native_status: if state_name.is_empty() {
                None
            } else {
                Some(state_name.to_string())
            },
            status_metadata: Value::Null,
            attachments: Vec::new(),
        })
    }

    /// Build the `IssueUpdateInput` for a [`SubjectPatch`].
    ///
    /// `status` translates to a Linear `stateId` (UUID) — the canonical key
    /// `issueUpdate(input: { stateId })` expects. The UUID comes from
    /// [`StatusMap::animus_to_linear_state_id`]. If no state in the team's
    /// workflow maps to the requested animus status, we return
    /// [`BackendError::InvalidRequest`] with a hint about [`crate::config::ENV_STATUS_MAP`].
    fn build_update_input(
        patch: &SubjectPatch,
        status_map: &StatusMap,
    ) -> Result<Value, BackendError> {
        let mut input = serde_json::Map::new();

        if let Some(Some(assignee)) = &patch.assignee {
            input.insert("assigneeId".to_string(), Value::String(assignee.clone()));
        } else if let Some(None) = &patch.assignee {
            input.insert("assigneeId".to_string(), Value::Null);
        }

        if !patch.labels_add.is_empty() {
            input.insert("addedLabelIds".to_string(), json!(patch.labels_add));
        }
        if !patch.labels_remove.is_empty() {
            input.insert("removedLabelIds".to_string(), json!(patch.labels_remove));
        }

        // NOTE: `patch.comment` is intentionally NOT mapped to `description`
        // here — that would overwrite the issue body. Linear treats comments
        // as a separate activity stream, so [`SubjectBackend::update`] handles
        // them via a follow-up [`Self::comment_mutation`] (`commentCreate`)
        // GraphQL call after the `issueUpdate` succeeds.

        for (key, value) in &patch.custom {
            input.insert(key.clone(), value.clone());
        }

        if let Some(status) = patch.status {
            let state_id = status_map.animus_to_linear_state_id(status).ok_or_else(|| {
                BackendError::InvalidRequest(format!(
                    "no Linear workflow state maps to animus status {status:?}; configure LINEAR_STATUS_MAP env var to override"
                ))
            })?;
            input.insert("stateId".to_string(), Value::String(state_id.to_string()));
        }

        Ok(Value::Object(input))
    }

    /// Create a Linear issue from a flat [`CreateRequest`] via `issueCreate`.
    ///
    /// Inherent method (not a trait verb): `main.rs` registers it on the
    /// generic `Plugin` shell for the `issue/create` + `subject/create`
    /// methods. Reuses the same helpers as `update()`: `status_map`,
    /// `animus_to_linear_state_id`, `issue_to_subject`, `map_graphql_err`.
    pub async fn create(&self, req: CreateRequest) -> Result<Subject, BackendError> {
        if !self.client.has_token() {
            return Err(missing_token_error());
        }

        let title = req.title.trim();
        if title.is_empty() {
            return Err(BackendError::InvalidRequest(
                "title must not be empty".to_string(),
            ));
        }

        let team_id = req
            .team_id
            .clone()
            .or_else(|| self.client.team_id().map(str::to_string))
            .ok_or_else(|| {
                BackendError::InvalidRequest(
                    "LINEAR_TEAM_ID must be set to create issues".to_string(),
                )
            })?;

        if !req.labels.is_empty() {
            tracing::warn!(
                target: "animus_subject_linear",
                labels = ?req.labels,
                "issue/create received labels but label assignment on create is not \
                 supported in v0.1.8; creating the issue without them"
            );
        }

        // Only fetch the team's workflow map when a status was requested.
        // A status-less create still translates the *returned* issue via the
        // `WorkflowState.type` fallback in `issue_to_subject`, so an empty
        // map is sufficient there — and we avoid a wasted round-trip.
        let default_map;
        let status_map: &StatusMap = if req.status.is_some() {
            self.status_map().await?
        } else {
            default_map = StatusMap::default();
            &default_map
        };

        let mut input = serde_json::Map::new();
        input.insert("teamId".to_string(), json!(team_id));
        input.insert("title".to_string(), json!(title));

        if let Some(body) = req.body.as_deref().filter(|b| !b.is_empty()) {
            input.insert("description".to_string(), json!(body));
        }
        if let Some(project) = req
            .project_id
            .clone()
            .or_else(|| self.client.project_id().map(str::to_string))
        {
            input.insert("projectId".to_string(), json!(project));
        }
        if let Some(priority) = req.priority.as_deref().and_then(priority_bucket_to_linear) {
            input.insert("priority".to_string(), json!(priority));
        }
        if let Some(status_str) = req.status.as_deref() {
            let status = parse_subject_status(status_str).ok_or_else(|| {
                BackendError::InvalidRequest(format!("unknown status {status_str:?}"))
            })?;
            let state_id = status_map.animus_to_linear_state_id(status).ok_or_else(|| {
                BackendError::InvalidRequest(format!(
                    "no Linear workflow state maps to animus status {status:?}; configure LINEAR_STATUS_MAP env var to override"
                ))
            })?;
            input.insert("stateId".to_string(), json!(state_id));
        }

        let response = self
            .client
            .execute(&Self::create_mutation(), json!({ "input": input }))
            .await
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;
        let data = response.into_data().map_err(map_graphql_err)?;
        let payload = data.get("issueCreate").ok_or_else(|| {
            BackendError::Other(anyhow::anyhow!("missing `issueCreate` in response"))
        })?;
        let success = payload
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !success {
            return Err(BackendError::InvalidRequest(format!(
                "linear rejected issue create: {payload}"
            )));
        }
        let issue = payload
            .get("issue")
            .filter(|v| !v.is_null())
            .cloned()
            .ok_or_else(|| {
                BackendError::Other(anyhow::anyhow!("issueCreate returned no issue node"))
            })?;

        Self::issue_to_subject(&issue, status_map)
    }
}

#[async_trait]
impl SubjectBackend for LinearBackend {
    async fn list(&self, filter: SubjectFilter) -> Result<SubjectList, BackendError> {
        if !self.client.has_token() {
            return Err(missing_token_error());
        }
        let status_map = self.status_map().await?;
        let issue_filter = self.build_issue_filter(&filter, status_map);
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
            subjects.push(Self::issue_to_subject(node, status_map)?);
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
        let status_map = self.status_map().await?;
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

        Self::issue_to_subject(issue, status_map)
    }

    async fn update(&self, id: &SubjectId, patch: SubjectPatch) -> Result<Subject, BackendError> {
        let native = Self::native_id(id)?;
        if !self.client.has_token() {
            return Err(missing_token_error());
        }
        let status_map = self.status_map().await?;
        let input = Self::build_update_input(&patch, status_map)?;
        let input_has_fields = input.as_object().map(|o| !o.is_empty()).unwrap_or(false);

        // Two paths into `issue`:
        //   1. The patch has at least one field — call `issueUpdate` and use
        //      the mutation's returned issue node.
        //   2. The patch is comment-only — `issueUpdate` with empty input is
        //      a no-op, so we fetch the issue with `AnimusGetIssue` instead.
        // Both paths yield the same `ISSUE_FIELDS` shape, including the
        // Linear-internal UUID used by `commentCreate`.
        let issue = if input_has_fields {
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

            payload
                .get("issue")
                .filter(|v| !v.is_null())
                .cloned()
                .ok_or_else(|| BackendError::NotFound(id.to_string()))?
        } else {
            let variables = json!({ "id": native });
            let response = self
                .client
                .execute(&Self::get_query(), variables)
                .await
                .map_err(|e| BackendError::Unavailable(e.to_string()))?;
            let data = response.into_data().map_err(map_graphql_err)?;
            data.get("issue")
                .filter(|v| !v.is_null())
                .cloned()
                .ok_or_else(|| BackendError::NotFound(id.to_string()))?
        };

        // `patch.comment` translates to a Linear comment, not a description
        // overwrite. Run the `commentCreate` mutation after the issue has
        // been resolved so we have the canonical UUID Linear expects on the
        // `issueId` argument.
        if let Some(body) = patch.comment.as_deref().filter(|c| !c.is_empty()) {
            let issue_uuid = issue.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
                BackendError::Other(anyhow::anyhow!(
                    "linear response for {id} is missing issue.id; cannot post comment"
                ))
            })?;
            let variables = json!({
                "input": { "issueId": issue_uuid, "body": body }
            });
            let response = self
                .client
                .execute(Self::comment_mutation(), variables)
                .await
                .map_err(|e| BackendError::Unavailable(e.to_string()))?;
            let data = response.into_data().map_err(map_graphql_err)?;
            let success = data
                .pointer("/commentCreate/success")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !success {
                return Err(BackendError::InvalidRequest(format!(
                    "linear rejected comment for {id}: {data}"
                )));
            }
        }

        Self::issue_to_subject(&issue, status_map)
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
            supports_create: true,
            supports_delete: false,
            supports_pagination: true,
            // `schema()` is sync; runtime discovery from Linear is async.
            // Surface a sensible static fallback that lists the well-known
            // Linear-default state names. If a workflow author needs the
            // team's actual states, they can call `list/get/update` once
            // (which populates the runtime-discovered map) and read the
            // names from issue.custom["linear_state_name"].
            native_status_values: status_map::FALLBACK_NATIVE_STATUSES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            status_dispatch_hints: Vec::new(),
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

/// Map an Animus priority bucket (`"p0"`..`"p3"`) to Linear's integer
/// `priority` field. Linear uses `0 = No priority, 1 = Urgent, 2 = High,
/// 3 = Normal, 4 = Low`; we map the four Animus buckets onto Linear's four
/// real priorities (`p0` = most urgent -> `1`), leaving Linear's `0` unused.
/// An unknown bucket returns `None`, so the field is omitted and Linear
/// applies the team default.
fn priority_bucket_to_linear(bucket: &str) -> Option<i64> {
    match bucket {
        "p0" | "P0" => Some(1),
        "p1" | "P1" => Some(2),
        "p2" | "P2" => Some(3),
        "p3" | "P3" => Some(4),
        _ => None,
    }
}

/// Whether Linear's priority integer must be *reversed* to land on Animus's
/// `Subject.priority` scale.
///
/// As of v0.1.8 the two scales run in OPPOSITE directions:
///   * Animus `Subject.priority` (`0..=4`): 0=none, 1=low, 2=medium, 3=high,
///     4=critical — higher number = more urgent.
///   * Linear's `priority` int: 0=None, 1=Urgent, 2=High, 3=Normal, 4=Low —
///     lower number (1) = more urgent.
///
/// So today this is `true`: Linear Urgent(1) maps to Animus critical(4), etc.,
/// keeping "most urgent" aligned across both (P0 = highest). If Animus ever
/// redefines its priority so the scales agree, flip this to `false` (Linear's
/// int is then used directly) and update `linear_priority_maps_to_animus_scale`.
const ANIMUS_PRIORITY_REVERSE: bool = true;

/// Map Linear's `priority` integer to Animus's `Subject.priority` (`0..=4`),
/// honoring [`ANIMUS_PRIORITY_REVERSE`]. Inputs outside `0..=4` return `None`
/// so the field is left unset rather than carrying a nonsense value.
fn linear_priority_to_animus(linear: u64) -> Option<u8> {
    let animus = if ANIMUS_PRIORITY_REVERSE {
        match linear {
            0 => 0, // None   -> none
            1 => 4, // Urgent -> critical
            2 => 3, // High   -> high
            3 => 2, // Normal -> medium
            4 => 1, // Low    -> low
            _ => return None,
        }
    } else {
        match linear {
            0..=4 => linear as u8,
            _ => return None,
        }
    };
    Some(animus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_buckets_map_to_linear_ints() {
        assert_eq!(priority_bucket_to_linear("p0"), Some(1));
        assert_eq!(priority_bucket_to_linear("p1"), Some(2));
        assert_eq!(priority_bucket_to_linear("p2"), Some(3));
        assert_eq!(priority_bucket_to_linear("p3"), Some(4));
        assert_eq!(priority_bucket_to_linear("urgent"), None);
    }

    #[test]
    fn linear_priority_maps_to_animus_scale() {
        // Pins the behavior under the current `ANIMUS_PRIORITY_REVERSE` value.
        // If that const is ever flipped, update these expectations to match.
        assert_eq!(linear_priority_to_animus(0), Some(0)); // None   -> none
        assert_eq!(linear_priority_to_animus(1), Some(4)); // Urgent -> critical
        assert_eq!(linear_priority_to_animus(2), Some(3)); // High   -> high
        assert_eq!(linear_priority_to_animus(3), Some(2)); // Normal -> medium
        assert_eq!(linear_priority_to_animus(4), Some(1)); // Low    -> low
        assert_eq!(linear_priority_to_animus(5), None); // out of range
    }
}
