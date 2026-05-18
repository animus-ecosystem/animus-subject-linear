//! Environment-driven configuration for the Linear backend plugin.
//!
//! All fields are populated from environment variables so the plugin can be
//! launched as a stdio child process without command-line argument plumbing.
//!
//! Loading is lenient: a missing [`ENV_API_TOKEN`] is allowed so credential-free
//! entry points (`--manifest`, `schema()`, `health()`) work without secrets in
//! the environment. The token is validated at the point of use inside
//! [`crate::backend::LinearBackend`]'s `list`/`get`/`update` methods.

use std::collections::HashMap;

use animus_subject_protocol::SubjectStatus;
use anyhow::Result;

/// Default Linear GraphQL endpoint.
pub const DEFAULT_LINEAR_API_URL: &str = "https://api.linear.app/graphql";

/// Environment variable holding the Linear personal API token.
pub const ENV_API_TOKEN: &str = "LINEAR_API_TOKEN";

/// Environment variable holding the Linear team UUID or key to scope queries to.
pub const ENV_TEAM_ID: &str = "LINEAR_TEAM_ID";

/// Environment variable overriding the GraphQL endpoint (used by tests + self-hosted Linear).
pub const ENV_API_URL: &str = "LINEAR_API_URL";

/// Environment variable holding a JSON object that overrides the type-based
/// auto-map for specific Linear state names. Example:
///
/// ```text
/// LINEAR_STATUS_MAP='{"Spec":"Ready","Implementation":"InProgress","Shipped":"Done"}'
/// ```
///
/// Keys are matched against `WorkflowState.name` (case-sensitive). Values must
/// be one of `Ready`, `InProgress`, `Blocked`, `Done`, `Cancelled`. Unknown
/// values are silently ignored (the rest of the map still applies); a malformed
/// JSON blob also falls back to the empty map rather than crashing the plugin.
pub const ENV_STATUS_MAP: &str = "LINEAR_STATUS_MAP";

/// Runtime configuration for the Linear backend plugin.
#[derive(Debug, Clone)]
pub struct LinearConfig {
    /// Personal Linear API token (sent as the `Authorization` header).
    ///
    /// `None` means the token wasn't set in the environment. The plugin can
    /// still answer `--manifest`, `schema()`, and `health()` in that state;
    /// `list`/`get`/`update` will return an error at call time.
    pub api_token: Option<String>,
    /// Linear GraphQL endpoint URL.
    pub api_url: String,
    /// Optional team identifier; when set, `list` queries are constrained to this team.
    pub team_id: Option<String>,
    /// User-supplied overrides for the status auto-map. See [`ENV_STATUS_MAP`].
    /// Keys are Linear `WorkflowState.name` strings (case-sensitive); values
    /// are the normalized [`SubjectStatus`] they should map to. Empty by
    /// default — the type-based auto-map handles every state on its own.
    pub status_overrides: HashMap<String, SubjectStatus>,
}

impl LinearConfig {
    /// Read the configuration from environment variables.
    ///
    /// Lenient: a missing [`ENV_API_TOKEN`] is not an error - it just results
    /// in `api_token: None`. This lets credential-free entry points like
    /// `--manifest` succeed without exporting a token.
    pub fn from_env() -> Result<Self> {
        let api_token = std::env::var(ENV_API_TOKEN).ok().filter(|s| !s.is_empty());
        let api_url =
            std::env::var(ENV_API_URL).unwrap_or_else(|_| DEFAULT_LINEAR_API_URL.to_string());
        let team_id = std::env::var(ENV_TEAM_ID).ok().filter(|s| !s.is_empty());
        let status_overrides = std::env::var(ENV_STATUS_MAP)
            .ok()
            .filter(|s| !s.is_empty())
            .map(|raw| parse_status_overrides(&raw))
            .unwrap_or_default();
        Ok(Self {
            api_token,
            api_url,
            team_id,
            status_overrides,
        })
    }

    /// Construct a config in-memory (used by tests and embedders that don't
    /// want to round-trip through process environment).
    pub fn new(
        api_token: impl Into<String>,
        api_url: impl Into<String>,
        team_id: Option<String>,
    ) -> Self {
        Self {
            api_token: Some(api_token.into()),
            api_url: api_url.into(),
            team_id,
            status_overrides: HashMap::new(),
        }
    }

    /// Construct a config without an API token, for callers that only need
    /// credential-free operations.
    pub fn without_token(api_url: impl Into<String>, team_id: Option<String>) -> Self {
        Self {
            api_token: None,
            api_url: api_url.into(),
            team_id,
            status_overrides: HashMap::new(),
        }
    }

    /// In-memory builder that also threads through a [`status_overrides`](Self::status_overrides)
    /// map. Mirrors what [`Self::from_env`] would build from [`ENV_STATUS_MAP`].
    pub fn with_status_overrides(mut self, overrides: HashMap<String, SubjectStatus>) -> Self {
        self.status_overrides = overrides;
        self
    }
}

/// Parse a JSON blob into a name -> [`SubjectStatus`] map. Malformed JSON or
/// values outside the supported [`SubjectStatus`] variants are silently
/// dropped (returning an empty map for a fully-invalid blob); we'd rather
/// the plugin keep running with the type-based auto-map than crash on a
/// typo in the env var.
fn parse_status_overrides(raw: &str) -> HashMap<String, SubjectStatus> {
    let parsed: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(raw) {
        Ok(map) => map,
        Err(err) => {
            tracing::warn!(
                target: "animus_subject_linear",
                ?err,
                "ignoring malformed LINEAR_STATUS_MAP env var"
            );
            return HashMap::new();
        }
    };

    let mut out = HashMap::with_capacity(parsed.len());
    for (name, value) in parsed {
        let status_str = match value.as_str() {
            Some(s) => s,
            None => {
                tracing::warn!(
                    target: "animus_subject_linear",
                    state = %name,
                    value = %value,
                    "LINEAR_STATUS_MAP entry must be a string; skipping"
                );
                continue;
            }
        };
        let status = match parse_subject_status(status_str) {
            Some(s) => s,
            None => {
                tracing::warn!(
                    target: "animus_subject_linear",
                    state = %name,
                    value = %status_str,
                    "LINEAR_STATUS_MAP value is not a known SubjectStatus; skipping"
                );
                continue;
            }
        };
        out.insert(name, status);
    }
    out
}

/// Map the PascalCase variant names (`"Ready"`, `"InProgress"`, ...) used in
/// `LINEAR_STATUS_MAP` to [`SubjectStatus`]. Also accepts the kebab-case
/// serde form (`"in-progress"`) for users who write the map as JSON output
/// from another tool.
fn parse_subject_status(raw: &str) -> Option<SubjectStatus> {
    match raw {
        "Ready" | "ready" => Some(SubjectStatus::Ready),
        "InProgress" | "in-progress" | "in_progress" => Some(SubjectStatus::InProgress),
        "Blocked" | "blocked" => Some(SubjectStatus::Blocked),
        "Done" | "done" => Some(SubjectStatus::Done),
        "Cancelled" | "cancelled" | "Canceled" | "canceled" => Some(SubjectStatus::Cancelled),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_builds_config_without_env() {
        let cfg = LinearConfig::new("tok", "http://localhost:1234/graphql", Some("ENG".into()));
        assert_eq!(cfg.api_token.as_deref(), Some("tok"));
        assert_eq!(cfg.api_url, "http://localhost:1234/graphql");
        assert_eq!(cfg.team_id.as_deref(), Some("ENG"));
    }

    #[test]
    fn without_token_yields_none() {
        let cfg = LinearConfig::without_token("http://localhost:1234/graphql", None);
        assert!(cfg.api_token.is_none());
        assert_eq!(cfg.api_url, "http://localhost:1234/graphql");
        assert!(cfg.team_id.is_none());
    }

    /// Regression: `--manifest` must work without `LINEAR_API_TOKEN`. The
    /// runtime parses argv before the backend is built, but the binary's
    /// `main` constructs the config first - so `from_env()` MUST be lenient.
    #[test]
    fn from_env_succeeds_without_token() {
        // SAFETY: tests in the same process share env. We snapshot, mutate,
        // run the assertion, then restore.
        let saved_token = std::env::var(ENV_API_TOKEN).ok();
        let saved_url = std::env::var(ENV_API_URL).ok();
        let saved_team = std::env::var(ENV_TEAM_ID).ok();
        let saved_status_map = std::env::var(ENV_STATUS_MAP).ok();

        std::env::remove_var(ENV_API_TOKEN);
        std::env::remove_var(ENV_API_URL);
        std::env::remove_var(ENV_TEAM_ID);
        std::env::remove_var(ENV_STATUS_MAP);

        let cfg = LinearConfig::from_env().expect("from_env must be lenient about missing token");
        assert!(
            cfg.api_token.is_none(),
            "missing token should map to None, not panic"
        );
        assert_eq!(cfg.api_url, DEFAULT_LINEAR_API_URL);
        assert!(cfg.team_id.is_none());
        assert!(cfg.status_overrides.is_empty());

        if let Some(v) = saved_token {
            std::env::set_var(ENV_API_TOKEN, v);
        }
        if let Some(v) = saved_url {
            std::env::set_var(ENV_API_URL, v);
        }
        if let Some(v) = saved_team {
            std::env::set_var(ENV_TEAM_ID, v);
        }
        if let Some(v) = saved_status_map {
            std::env::set_var(ENV_STATUS_MAP, v);
        }
    }

    #[test]
    fn parse_status_overrides_handles_valid_json() {
        let raw = r#"{"Spec":"Ready","Implementation":"InProgress","Shipped":"Done"}"#;
        let parsed = parse_status_overrides(raw);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed.get("Spec"), Some(&SubjectStatus::Ready));
        assert_eq!(
            parsed.get("Implementation"),
            Some(&SubjectStatus::InProgress)
        );
        assert_eq!(parsed.get("Shipped"), Some(&SubjectStatus::Done));
    }

    #[test]
    fn parse_status_overrides_ignores_unknown_values() {
        let raw = r#"{"Spec":"NotAStatus","Implementation":"InProgress"}"#;
        let parsed = parse_status_overrides(raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed.get("Implementation"),
            Some(&SubjectStatus::InProgress)
        );
    }

    #[test]
    fn parse_status_overrides_returns_empty_for_malformed_json() {
        assert!(parse_status_overrides("not json at all").is_empty());
        assert!(parse_status_overrides("[1,2,3]").is_empty());
    }
}
