//! Environment-driven configuration for the Linear backend plugin.
//!
//! All fields are populated from environment variables so the plugin can be
//! launched as a stdio child process without command-line argument plumbing.

use anyhow::{anyhow, Result};

/// Default Linear GraphQL endpoint.
pub const DEFAULT_LINEAR_API_URL: &str = "https://api.linear.app/graphql";

/// Environment variable holding the Linear personal API token.
pub const ENV_API_TOKEN: &str = "LINEAR_API_TOKEN";

/// Environment variable holding the Linear team UUID or key to scope queries to.
pub const ENV_TEAM_ID: &str = "LINEAR_TEAM_ID";

/// Environment variable overriding the GraphQL endpoint (used by tests + self-hosted Linear).
pub const ENV_API_URL: &str = "LINEAR_API_URL";

/// Runtime configuration for the Linear backend plugin.
#[derive(Debug, Clone)]
pub struct LinearConfig {
    /// Personal Linear API token (sent as the `Authorization` header).
    pub api_token: String,
    /// Linear GraphQL endpoint URL.
    pub api_url: String,
    /// Optional team identifier; when set, `list` queries are constrained to this team.
    pub team_id: Option<String>,
}

impl LinearConfig {
    /// Read the configuration from environment variables. Returns an error if
    /// the required [`ENV_API_TOKEN`] is missing.
    pub fn from_env() -> Result<Self> {
        let api_token = std::env::var(ENV_API_TOKEN).map_err(|_| {
            anyhow!(
                "{ENV_API_TOKEN} is not set; cannot start animus-subject-linear without a Linear API token"
            )
        })?;
        let api_url =
            std::env::var(ENV_API_URL).unwrap_or_else(|_| DEFAULT_LINEAR_API_URL.to_string());
        let team_id = std::env::var(ENV_TEAM_ID).ok().filter(|s| !s.is_empty());
        Ok(Self {
            api_token,
            api_url,
            team_id,
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
            api_token: api_token.into(),
            api_url: api_url.into(),
            team_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_builds_config_without_env() {
        let cfg = LinearConfig::new("tok", "http://localhost:1234/graphql", Some("ENG".into()));
        assert_eq!(cfg.api_token, "tok");
        assert_eq!(cfg.api_url, "http://localhost:1234/graphql");
        assert_eq!(cfg.team_id.as_deref(), Some("ENG"));
    }
}
