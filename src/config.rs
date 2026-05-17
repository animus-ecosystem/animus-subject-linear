//! Environment-driven configuration for the Linear backend plugin.
//!
//! All fields are populated from environment variables so the plugin can be
//! launched as a stdio child process without command-line argument plumbing.
//!
//! Loading is lenient: a missing [`ENV_API_TOKEN`] is allowed so credential-free
//! entry points (`--manifest`, `schema()`, `health()`) work without secrets in
//! the environment. The token is validated at the point of use inside
//! [`crate::backend::LinearBackend`]'s `list`/`get`/`update` methods.

use anyhow::Result;

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
    ///
    /// `None` means the token wasn't set in the environment. The plugin can
    /// still answer `--manifest`, `schema()`, and `health()` in that state;
    /// `list`/`get`/`update` will return an error at call time.
    pub api_token: Option<String>,
    /// Linear GraphQL endpoint URL.
    pub api_url: String,
    /// Optional team identifier; when set, `list` queries are constrained to this team.
    pub team_id: Option<String>,
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
            api_token: Some(api_token.into()),
            api_url: api_url.into(),
            team_id,
        }
    }

    /// Construct a config without an API token, for callers that only need
    /// credential-free operations.
    pub fn without_token(api_url: impl Into<String>, team_id: Option<String>) -> Self {
        Self {
            api_token: None,
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

        std::env::remove_var(ENV_API_TOKEN);
        std::env::remove_var(ENV_API_URL);
        std::env::remove_var(ENV_TEAM_ID);

        let cfg = LinearConfig::from_env().expect("from_env must be lenient about missing token");
        assert!(
            cfg.api_token.is_none(),
            "missing token should map to None, not panic"
        );
        assert_eq!(cfg.api_url, DEFAULT_LINEAR_API_URL);
        assert!(cfg.team_id.is_none());

        if let Some(v) = saved_token {
            std::env::set_var(ENV_API_TOKEN, v);
        }
        if let Some(v) = saved_url {
            std::env::set_var(ENV_API_URL, v);
        }
        if let Some(v) = saved_team {
            std::env::set_var(ENV_TEAM_ID, v);
        }
    }
}
