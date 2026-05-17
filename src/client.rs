//! Thin reqwest wrapper around Linear's GraphQL HTTP endpoint.
//!
//! The client speaks only what this plugin needs:
//!
//! - `viewer` for health checks
//! - `issues` for `subject/list`
//! - `issue(id)` for `subject/get`
//! - `issueUpdate` for `subject/update`
//! - `workflowStates` for status_id translation when writing `state` updates
//!
//! Queries are hand-rolled JSON rather than going through `graphql_client`
//! because the surface is tiny and dropping the build-time schema dependency
//! keeps the repo skeleton minimal.

use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

use crate::config::LinearConfig;

/// HTTP timeout for individual GraphQL requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Outcome of a single GraphQL POST. We surface `errors` so the backend can
/// translate them into [`animus_subject_protocol::BackendError`] variants
/// (auth -> `PermissionDenied`, not-found -> `NotFound`, etc.).
#[derive(Debug)]
pub struct GraphQlResponse {
    /// HTTP status code returned by Linear.
    pub status: StatusCode,
    /// `data` field of the GraphQL response, if present.
    pub data: Option<Value>,
    /// `errors` field of the GraphQL response, if present.
    pub errors: Vec<GraphQlError>,
}

impl GraphQlResponse {
    /// Returns `Ok(data)` if the request succeeded and `data` is present.
    /// Returns an `Err` with a human-readable message otherwise.
    pub fn into_data(self) -> Result<Value> {
        if !self.errors.is_empty() {
            let messages: Vec<String> = self
                .errors
                .iter()
                .map(|error| error.message.clone())
                .collect();
            return Err(anyhow!("linear graphql errors: {}", messages.join("; ")));
        }
        if !self.status.is_success() {
            return Err(anyhow!("linear http {}: {:?}", self.status, self.data));
        }
        self.data
            .ok_or_else(|| anyhow!("linear response missing `data` field"))
    }
}

/// One entry in the GraphQL response's `errors` array.
#[derive(Debug, Clone, Deserialize)]
pub struct GraphQlError {
    /// Human-readable description.
    pub message: String,
    /// Optional structured detail (Linear-specific keys like `userPresentableMessage`).
    #[serde(default)]
    pub extensions: Option<Value>,
}

/// HTTP client for Linear's GraphQL API.
#[derive(Debug, Clone)]
pub struct LinearClient {
    http: Client,
    api_url: String,
    team_id: Option<String>,
}

impl LinearClient {
    /// Construct a client from configuration. Builds the underlying reqwest
    /// client with the `Authorization` header pre-set so individual call sites
    /// don't have to remember to attach it.
    pub fn new(config: &LinearConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&config.api_token)
            .context("LINEAR_API_TOKEN contains characters that aren't valid in a header")?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let http = Client::builder()
            .default_headers(headers)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("animus-subject-linear/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build reqwest client")?;

        Ok(Self {
            http,
            api_url: config.api_url.clone(),
            team_id: config.team_id.clone(),
        })
    }

    /// Team scope for `list` queries, if configured.
    pub fn team_id(&self) -> Option<&str> {
        self.team_id.as_deref()
    }

    /// POST a raw GraphQL query string + variables and return the parsed
    /// response. Network/IO errors bubble up; GraphQL-level errors are returned
    /// inside [`GraphQlResponse::errors`] so callers can react to them.
    pub async fn execute(&self, query: &str, variables: Value) -> Result<GraphQlResponse> {
        let body = json!({ "query": query, "variables": variables });
        let response = self
            .http
            .post(&self.api_url)
            .json(&body)
            .send()
            .await
            .context("linear graphql request failed at the transport layer")?;

        let status = response.status();
        let raw: Value = response
            .json()
            .await
            .context("linear response was not valid JSON")?;

        let data = raw.get("data").cloned().filter(|v| !v.is_null());
        let errors: Vec<GraphQlError> = raw
            .get("errors")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        Ok(GraphQlResponse {
            status,
            data,
            errors,
        })
    }
}
