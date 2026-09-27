//! Configuration schema: the shape users write in YAML.
//!
//! This is deliberately separate from the compiled runtime form in [`crate::snapshot`].
//! The schema is optimised for humans (names, strings, defaults); the snapshot is optimised
//! for the request path (pre-parsed URIs, a routing trie, shared `Arc`s). Compiling one into
//! the other is also where validation happens, so an invalid config never reaches traffic.

use std::{collections::BTreeMap, net::SocketAddr, path::Path};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listeners: Listeners,
    /// Named upstream clusters. A `BTreeMap` keeps admin output and error ordering stable.
    #[serde(default)]
    pub clusters: BTreeMap<String, ClusterConfig>,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listeners {
    pub data: ListenerConfig,
    pub admin: ListenerConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerConfig {
    pub bind: SocketAddr,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    /// Base URLs such as `http://users-1:8080` (scheme + authority, no path).
    pub endpoints: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub id: String,
    #[serde(rename = "match")]
    pub matcher: MatchConfig,
    pub cluster: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    /// Exact host (case-insensitive, port ignored). `None` matches any host that has no
    /// host-specific routes of its own.
    #[serde(default)]
    pub host: Option<String>,
    /// Segment-aligned prefix: `/users` matches `/users` and `/users/42`, not `/usersx`.
    pub path_prefix: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing config: {0}")]
    Parse(String),
    #[error("route `{route}`: {reason}")]
    Route { route: String, reason: String },
    #[error("cluster `{cluster}`: {reason}")]
    Cluster { cluster: String, reason: String },
}

impl Config {
    pub fn from_yaml(text: &str) -> Result<Self, ConfigError> {
        serde_saphyr::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_yaml(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
listeners:
  data:
    bind: "0.0.0.0:8080"
  admin:
    bind: "127.0.0.1:9901"
clusters:
  users:
    endpoints: ["http://users-1:8080", "http://users-2:8080"]
routes:
  - id: users-api
    match:
      host: api.example.com
      path_prefix: /users
    cluster: users
"#;

    #[test]
    fn parses_example() {
        let config = Config::from_yaml(EXAMPLE).unwrap();
        assert_eq!(config.clusters["users"].endpoints.len(), 2);
        assert_eq!(
            config.routes[0].matcher.host.as_deref(),
            Some("api.example.com")
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        // Typos like `path_prefx` must fail loudly instead of silently matching everything.
        let bad = EXAMPLE.replace("path_prefix", "path_prefx");
        assert!(matches!(
            Config::from_yaml(&bad),
            Err(ConfigError::Parse(_))
        ));
    }
}
