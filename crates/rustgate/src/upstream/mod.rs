//! Upstream clusters, endpoints and the pooled HTTP client.
//!
//! Phase 1 keeps this minimal: a cluster is a list of endpoints and `pick` returns the first.
//! Phase 2 adds per-endpoint runtime state (in-flight counter, health, breaker) and load
//! balancing; that state lives on [`Endpoint`] because every one of those features reads and
//! writes it.
//!
//! Endpoints are *interned* in an [`EndpointRegistry`] rather than owned by a snapshot. A
//! config reload compiles a new snapshot, but an endpoint that appears in both the old and
//! new config resolves to the same `Arc<Endpoint>`, so its health, breaker state and
//! in-flight count carry over instead of resetting on every reload. The same holds across
//! clusters: two clusters listing one URL share one endpoint, because they share one backend.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError, Weak},
    time::Duration,
};

use http::{
    Uri,
    uri::{Authority, InvalidUriParts, PathAndQuery, Scheme},
};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioTimer},
};

use crate::{body::ProxyBody, config::ClusterConfig, config::ConfigError};

/// TODO(phase 5): becomes part of the per-route timeout policy.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// One client for all upstreams. It is a cheap handle (`Clone` is an `Arc` bump) around a
/// connection pool keyed by (scheme, authority), so connections are reused per endpoint.
pub type HttpClient = Client<HttpConnector, ProxyBody>;

pub fn build_client() -> HttpClient {
    let mut connector = HttpConnector::new();
    // Proxied requests are latency-sensitive and already written in whole frames by hyper;
    // Nagle's algorithm would only add delay.
    connector.set_nodelay(true);
    connector.set_connect_timeout(Some(CONNECT_TIMEOUT));
    connector.enforce_http(true);
    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        // Without a timer the pool never evicts idle connections.
        .pool_timer(TokioTimer::new())
        .build(connector)
}

#[derive(Debug)]
pub struct Cluster {
    pub name: Arc<str>,
    pub endpoints: Vec<Arc<Endpoint>>,
}

/// Interns endpoints by normalized URL. Control plane only: it is touched while compiling a
/// snapshot, never per request, so a plain `Mutex` is fine.
///
/// Entries are `Weak`: once no snapshot references an endpoint (it was removed from config
/// and every in-flight request using it has finished), its state is dropped.
#[derive(Debug, Default)]
pub struct EndpointRegistry {
    endpoints: Mutex<HashMap<Box<str>, Weak<Endpoint>>>,
}

impl EndpointRegistry {
    pub fn resolve(&self, raw: &str) -> Result<Arc<Endpoint>, String> {
        let parsed = Endpoint::parse(raw)?;
        let mut endpoints = self
            .endpoints
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        endpoints.retain(|_, weak| weak.strong_count() > 0);
        if let Some(existing) = endpoints.get(parsed.url()).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        let endpoint = Arc::new(parsed);
        endpoints.insert(endpoint.url.clone(), Arc::downgrade(&endpoint));
        Ok(endpoint)
    }
}

impl Cluster {
    pub fn from_config(
        name: &str,
        config: &ClusterConfig,
        registry: &EndpointRegistry,
    ) -> Result<Self, ConfigError> {
        let err = |reason: String| ConfigError::Cluster {
            cluster: name.to_owned(),
            reason,
        };
        if config.endpoints.is_empty() {
            return Err(err("must have at least one endpoint".into()));
        }
        let mut endpoints: Vec<Arc<Endpoint>> = Vec::with_capacity(config.endpoints.len());
        for raw in &config.endpoints {
            let endpoint = registry.resolve(raw).map_err(err)?;
            if endpoints.iter().any(|e| Arc::ptr_eq(e, &endpoint)) {
                return Err(err(format!("duplicate endpoint {raw}")));
            }
            endpoints.push(endpoint);
        }
        Ok(Self {
            name: name.into(),
            endpoints,
        })
    }

    /// TODO(phase 2): load balancing across healthy endpoints.
    pub fn pick(&self) -> Option<&Arc<Endpoint>> {
        self.endpoints.first()
    }
}

#[derive(Debug)]
pub struct Endpoint {
    /// Normalized `scheme://authority`, lowercase. Also the registry key.
    url: Box<str>,
    scheme: Scheme,
    authority: Authority,
}

impl Endpoint {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let uri: Uri = raw
            .parse()
            .map_err(|e| format!("invalid endpoint URL {raw:?}: {e}"))?;
        let scheme = uri
            .scheme()
            .cloned()
            .ok_or_else(|| format!("endpoint {raw:?} has no scheme"))?;
        if scheme != Scheme::HTTP {
            return Err(format!(
                "endpoint {raw:?}: only http:// upstreams are supported so far"
            ));
        }
        let authority = uri
            .authority()
            .cloned()
            .ok_or_else(|| format!("endpoint {raw:?} has no host"))?;
        if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
            return Err(format!("endpoint {raw:?} must not include a path or query"));
        }
        Ok(Self {
            url: format!("{scheme}://{authority}")
                .to_ascii_lowercase()
                .into(),
            scheme,
            authority,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Builds the absolute upstream URI for a request. `Scheme` and `Authority` are
    /// `Bytes`-backed, so cloning them is a refcount bump, not a string copy.
    pub fn uri_for(&self, path_and_query: Option<&PathAndQuery>) -> Result<Uri, InvalidUriParts> {
        let mut parts = http::uri::Parts::default();
        parts.scheme = Some(self.scheme.clone());
        parts.authority = Some(self.authority.clone());
        parts.path_and_query = Some(
            path_and_query
                .cloned()
                .unwrap_or_else(|| PathAndQuery::from_static("/")),
        );
        Uri::from_parts(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_endpoints() {
        let e = Endpoint::parse("http://users-1:8080").unwrap();
        let pq = PathAndQuery::from_static("/users/1?x=2");
        assert_eq!(
            e.uri_for(Some(&pq)).unwrap(),
            "http://users-1:8080/users/1?x=2"
        );
        assert!(Endpoint::parse("http://users-1:8080/").is_ok());
    }

    #[test]
    fn registry_interns_endpoints_by_normalized_url() {
        let registry = EndpointRegistry::default();
        let a = registry.resolve("http://Users-1:8080").unwrap();
        let b = registry.resolve("http://users-1:8080/").unwrap();
        let other = registry.resolve("http://users-2:8080").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &other));
        assert_eq!(a.url(), "http://users-1:8080");

        // Once nothing references an endpoint, its state is gone and a new one is created.
        let weak = Arc::downgrade(&a);
        drop((a, b));
        assert!(weak.upgrade().is_none());
        registry.resolve("http://users-1:8080").unwrap();
    }

    #[test]
    fn rejects_invalid_endpoints() {
        for bad in [
            "users-1:8080",
            "https://users-1",
            "http://users-1/api",
            "http://u?x=1",
            "/path",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad} should be rejected");
        }
    }
}
