//! The compiled, immutable view of configuration that the data plane reads.
//!
//! A `Snapshot` is built once per config version and never mutated. It is published through
//! an `ArcSwap`, so:
//! - request handling reads it without locks (no `RwLock` reader contention, and a writer
//!   can never stall readers);
//! - a config change is a single atomic pointer swap;
//! - in-flight requests keep whatever route they matched (they hold an `Arc<Route>`), so a
//!   reload never changes a request halfway through.
//!
//! Phase 1 only loads config at startup, but going through the swap from day one means hot
//! reload (Phase 8) needs no changes to the request path.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use arc_swap::ArcSwap;

use crate::{
    config::{Config, ConfigError},
    routing::Router,
    upstream::{Cluster, EndpointRegistry},
};

pub type SharedSnapshot = Arc<ArcSwap<Snapshot>>;

#[derive(Debug)]
pub struct Snapshot {
    pub version: u64,
    pub router: Router<Arc<Route>>,
    /// In config order, for admin listing.
    pub routes: Vec<Arc<Route>>,
    pub clusters: BTreeMap<Arc<str>, Arc<Cluster>>,
}

#[derive(Debug)]
pub struct Route {
    pub id: Arc<str>,
    pub host: Option<String>,
    pub path_prefix: String,
    /// The route owns a strong reference to its cluster, so a request holding only its
    /// `Arc<Route>` keeps everything it needs alive even after the snapshot is replaced.
    pub cluster: Arc<Cluster>,
}

impl Snapshot {
    /// Builds a snapshot, resolving endpoints through `registry` so runtime state is shared
    /// with any previous snapshot compiled against the same registry.
    pub fn compile(
        config: &Config,
        version: u64,
        registry: &EndpointRegistry,
    ) -> Result<Self, ConfigError> {
        let mut clusters = BTreeMap::new();
        for (name, cluster_config) in &config.clusters {
            let cluster = Cluster::from_config(name, cluster_config, registry)?;
            clusters.insert(Arc::clone(&cluster.name), Arc::new(cluster));
        }

        let mut router = Router::default();
        let mut all_routes = Vec::with_capacity(config.routes.len());
        let mut ids = HashSet::new();
        for rc in &config.routes {
            let err = |reason: String| ConfigError::Route {
                route: rc.id.clone(),
                reason,
            };
            if !ids.insert(rc.id.as_str()) {
                return Err(err("duplicate route id".into()));
            }
            let cluster = clusters
                .get(rc.cluster.as_str())
                .ok_or_else(|| err(format!("unknown cluster `{}`", rc.cluster)))?;
            let route = Arc::new(Route {
                id: rc.id.as_str().into(),
                host: rc.matcher.host.clone(),
                path_prefix: rc.matcher.path_prefix.clone(),
                cluster: Arc::clone(cluster),
            });
            router
                .insert(
                    rc.matcher.host.as_deref(),
                    &rc.matcher.path_prefix,
                    Arc::clone(&route),
                )
                .map_err(|e| err(e.to_string()))?;
            all_routes.push(route);
        }

        Ok(Self {
            version,
            router,
            routes: all_routes,
            clusters,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(routes: &str) -> Result<Snapshot, ConfigError> {
        let yaml = format!(
            r#"
listeners:
  data: {{ bind: "127.0.0.1:0" }}
  admin: {{ bind: "127.0.0.1:0" }}
clusters:
  users: {{ endpoints: ["http://127.0.0.1:1"] }}
routes:
{routes}"#
        );
        Snapshot::compile(&Config::from_yaml(&yaml)?, 1, &EndpointRegistry::default())
    }

    #[test]
    fn compiles_valid_config() {
        let snap =
            compile("  - { id: a, match: { path_prefix: /users }, cluster: users }\n").unwrap();
        assert_eq!(snap.routes.len(), 1);
        assert_eq!(&*snap.router.lookup(None, "/users/1").unwrap().id, "a");
    }

    #[test]
    fn recompiling_keeps_endpoint_identity() {
        let config = Config::from_yaml(
            r#"
listeners:
  data: { bind: "127.0.0.1:0" }
  admin: { bind: "127.0.0.1:0" }
clusters:
  users: { endpoints: ["http://127.0.0.1:1"] }
  users-canary: { endpoints: ["http://127.0.0.1:1"] }
"#,
        )
        .unwrap();
        let registry = EndpointRegistry::default();
        let v1 = Snapshot::compile(&config, 1, &registry).unwrap();
        let v2 = Snapshot::compile(&config, 2, &registry).unwrap();
        let endpoint =
            |snap: &Snapshot, cluster: &str| Arc::clone(&snap.clusters[cluster].endpoints[0]);
        // Across reloads and across clusters, one backend is one `Endpoint`.
        assert!(Arc::ptr_eq(
            &endpoint(&v1, "users"),
            &endpoint(&v2, "users")
        ));
        assert!(Arc::ptr_eq(
            &endpoint(&v2, "users"),
            &endpoint(&v2, "users-canary")
        ));
    }

    #[test]
    fn rejects_unknown_cluster_and_duplicates() {
        let unknown = compile("  - { id: a, match: { path_prefix: / }, cluster: nope }\n");
        assert!(unknown.unwrap_err().to_string().contains("unknown cluster"));

        let dup_id = compile(
            "  - { id: a, match: { path_prefix: /x }, cluster: users }\n  - { id: a, match: { path_prefix: /y }, cluster: users }\n",
        );
        assert!(
            dup_id
                .unwrap_err()
                .to_string()
                .contains("duplicate route id")
        );

        let dup_match = compile(
            "  - { id: a, match: { path_prefix: /x }, cluster: users }\n  - { id: b, match: { path_prefix: /x/ }, cluster: users }\n",
        );
        assert!(
            dup_match
                .unwrap_err()
                .to_string()
                .contains("already matches")
        );
    }
}
