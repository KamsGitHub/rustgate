//! Host + path-prefix routing.
//!
//! Lookup is two-level:
//! 1. **Host**: exact, case-insensitive, port ignored. If the request's host has
//!    host-specific routes, *only* those are considered; otherwise the host-less routes are.
//!    There is no fallback between the two (Envoy virtual-host semantics): adding a route for
//!    `api.example.com` can never make it silently inherit an unrelated catch-all route.
//! 2. **Path**: a trie keyed by path *segments*, walked once per request, returning the
//!    longest matching prefix. Cost is O(number of segments in the path), independent of the
//!    number of routes.
//!
//! PERF: each step is a `HashMap` lookup (hashing the segment). For a handful of routes a
//! linear scan would be just as fast; the trie is chosen for predictable cost as the route
//! table grows. Measure before replacing it with something cleverer (e.g. a radix tree).

use std::{borrow::Cow, collections::HashMap};

/// Maps `(host, path)` to a value — an `Arc<Route>` in the gateway.
#[derive(Debug)]
pub struct Router<T> {
    hosts: HashMap<Box<str>, PathTrie<T>>,
    any_host: PathTrie<T>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouterError {
    #[error("path prefix must start with '/', got {0:?}")]
    InvalidPrefix(String),
    #[error("path prefix {0:?} contains a '.' or '..' segment")]
    DotSegment(String),
    #[error("another route already matches host {host:?} and prefix {prefix:?}")]
    Duplicate {
        host: Option<String>,
        prefix: String,
    },
}

impl<T> Default for Router<T> {
    fn default() -> Self {
        Self {
            hosts: HashMap::new(),
            any_host: PathTrie::default(),
        }
    }
}

impl<T> Router<T> {
    /// Adds a route. Prefixes are compared by segments, so `/users` and `/users/` are the
    /// same prefix and inserting both is a [`RouterError::Duplicate`].
    pub fn insert(
        &mut self,
        host: Option<&str>,
        prefix: &str,
        value: T,
    ) -> Result<(), RouterError> {
        if !prefix.starts_with('/') {
            return Err(RouterError::InvalidPrefix(prefix.to_owned()));
        }
        if !is_safe_path(prefix) {
            return Err(RouterError::DotSegment(prefix.to_owned()));
        }
        let trie = match host {
            Some(h) => self.hosts.entry(normalize_host(h).into()).or_default(),
            None => &mut self.any_host,
        };
        if trie.insert(prefix, value) {
            Ok(())
        } else {
            Err(RouterError::Duplicate {
                host: host.map(str::to_owned),
                prefix: prefix.to_owned(),
            })
        }
    }

    /// Finds the route for a request. `host` may include a port; `path` must not include the
    /// query string.
    pub fn lookup(&self, host: Option<&str>, path: &str) -> Option<&T> {
        let trie = host
            .and_then(|h| self.hosts.get(normalize_host(h).as_ref()))
            .unwrap_or(&self.any_host);
        trie.lookup(path)
    }
}

/// Returns false if the path has a `.` or `..` segment, including percent-encoded forms
/// (`%2e`, `%2E`).
///
/// Security: routing happens on the raw path, but upstreams commonly normalise dot-segments.
/// Forwarding `/public/../admin` would match the `/public` route (and its auth/rate-limit
/// policy) while the backend serves `/admin`. Rejecting these outright is the simplest rule
/// that cannot be bypassed.
pub fn is_safe_path(path: &str) -> bool {
    !path.split('/').any(is_dot_segment)
}

fn is_dot_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let mut dots = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'.' {
            i += 1;
        } else if bytes[i] == b'%'
            && bytes.get(i + 1) == Some(&b'2')
            && bytes.get(i + 2).map(u8::to_ascii_lowercase) == Some(b'e')
        {
            i += 3;
        } else {
            return false;
        }
        dots += 1;
    }
    matches!(dots, 1 | 2)
}

/// Lowercases (only if needed — no allocation in the common case), strips the port and a
/// trailing dot. Handles bracketed IPv6 literals like `[::1]:8080`.
fn normalize_host(host: &str) -> Cow<'_, str> {
    let host = strip_port(host);
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(host.to_ascii_lowercase())
    } else {
        Cow::Borrowed(host)
    }
}

fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return match rest.find(']') {
            Some(end) => &host[..end + 2],
            None => host,
        };
    }
    match host.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    }
}

fn segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|s| !s.is_empty())
}

#[derive(Debug)]
struct PathTrie<T> {
    root: Node<T>,
}

#[derive(Debug)]
struct Node<T> {
    value: Option<T>,
    children: HashMap<Box<str>, Node<T>>,
}

impl<T> Default for PathTrie<T> {
    fn default() -> Self {
        Self {
            root: Node::default(),
        }
    }
}

impl<T> Default for Node<T> {
    fn default() -> Self {
        Self {
            value: None,
            children: HashMap::new(),
        }
    }
}

impl<T> PathTrie<T> {
    /// Returns false if the prefix is already taken.
    fn insert(&mut self, prefix: &str, value: T) -> bool {
        let mut node = &mut self.root;
        for seg in segments(prefix) {
            node = node.children.entry(seg.into()).or_default();
        }
        if node.value.is_some() {
            return false;
        }
        node.value = Some(value);
        true
    }

    fn lookup(&self, path: &str) -> Option<&T> {
        let mut node = &self.root;
        let mut best = node.value.as_ref();
        for seg in segments(path) {
            let Some(child) = node.children.get(seg) else {
                break;
            };
            node = child;
            if child.value.is_some() {
                best = child.value.as_ref();
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn router(routes: &[(Option<&str>, &str, &'static str)]) -> Router<&'static str> {
        let mut r = Router::default();
        for (host, prefix, name) in routes {
            r.insert(*host, prefix, *name).unwrap();
        }
        r
    }

    #[test]
    fn longest_prefix_wins_on_segment_boundaries() {
        let r = router(&[
            (None, "/", "root"),
            (None, "/users", "users"),
            (None, "/users/admin", "admin"),
        ]);
        assert_eq!(r.lookup(None, "/users"), Some(&"users"));
        assert_eq!(r.lookup(None, "/users/"), Some(&"users"));
        assert_eq!(r.lookup(None, "/users/42"), Some(&"users"));
        assert_eq!(r.lookup(None, "/users/admin/x"), Some(&"admin"));
        assert_eq!(r.lookup(None, "/usersx"), Some(&"root"));
        assert_eq!(r.lookup(None, "/"), Some(&"root"));
    }

    #[test]
    fn no_match_without_root_route() {
        let r = router(&[(None, "/users", "users")]);
        assert_eq!(r.lookup(None, "/orders"), None);
        assert_eq!(r.lookup(None, "/"), None);
    }

    #[test]
    fn host_routes_are_exclusive_and_normalized() {
        let r = router(&[
            (Some("api.example.com"), "/", "api"),
            (None, "/", "default"),
            (None, "/health", "health"),
        ]);
        assert_eq!(r.lookup(Some("api.example.com"), "/x"), Some(&"api"));
        assert_eq!(r.lookup(Some("API.Example.COM:8443"), "/x"), Some(&"api"));
        assert_eq!(r.lookup(Some("api.example.com."), "/x"), Some(&"api"));
        // No fallback: the catch-all /health is not visible from a host with its own routes.
        assert_eq!(r.lookup(Some("api.example.com"), "/health"), Some(&"api"));
        assert_eq!(r.lookup(Some("other.com"), "/health"), Some(&"health"));
        assert_eq!(r.lookup(None, "/x"), Some(&"default"));
    }

    #[test]
    fn strips_ports_including_ipv6() {
        assert_eq!(strip_port("[::1]:8080"), "[::1]");
        assert_eq!(strip_port("[::1]"), "[::1]");
        assert_eq!(strip_port("example.com:80"), "example.com");
        assert_eq!(strip_port("example.com"), "example.com");
    }

    #[test]
    fn rejects_bad_prefixes_and_duplicates() {
        let mut r = Router::default();
        assert_eq!(
            r.insert(None, "users", 1),
            Err(RouterError::InvalidPrefix("users".into()))
        );
        assert!(matches!(
            r.insert(None, "/a/../b", 1),
            Err(RouterError::DotSegment(_))
        ));
        r.insert(None, "/users", 1).unwrap();
        assert!(matches!(
            r.insert(None, "/users/", 2),
            Err(RouterError::Duplicate { .. })
        ));
        // Same prefix on a different host is fine.
        r.insert(Some("a.com"), "/users", 3).unwrap();
    }

    #[test]
    fn detects_dot_segments() {
        for bad in [
            "/a/../b",
            "/..",
            "/a/.",
            "/%2e%2e/admin",
            "/a/%2E./b",
            "/.%2e",
        ] {
            assert!(!is_safe_path(bad), "{bad} should be rejected");
        }
        for ok in [
            "/",
            "/a/b",
            "/a..b",
            "/.well-known/x",
            "/a/...",
            "/%2e%2e%2e",
            "/v1.2",
        ] {
            assert!(is_safe_path(ok), "{ok} should be allowed");
        }
    }

    fn segs(max: usize) -> impl Strategy<Value = Vec<&'static str>> {
        prop::collection::vec(prop::sample::select(vec!["a", "b", "c"]), 0..=max)
    }

    proptest! {
        /// The trie must agree with the obvious O(routes) implementation: among inserted
        /// prefixes whose segments are a prefix of the path's segments, pick the longest.
        #[test]
        fn matches_linear_scan_reference(prefixes in prop::collection::vec(segs(3), 0..12), path in segs(5)) {
            let mut router = Router::default();
            let mut inserted: Vec<(Vec<&str>, usize)> = Vec::new();
            for (i, p) in prefixes.iter().enumerate() {
                let ok = router.insert(None, &format!("/{}", p.join("/")), i).is_ok();
                let is_new = !inserted.iter().any(|(q, _)| q == p);
                prop_assert_eq!(ok, is_new);
                if ok {
                    inserted.push((p.clone(), i));
                }
            }
            let expected = inserted
                .iter()
                .filter(|(p, _)| path.starts_with(p))
                .max_by_key(|(p, _)| p.len())
                .map(|(_, i)| *i);
            prop_assert_eq!(router.lookup(None, &format!("/{}", path.join("/"))).copied(), expected);
        }
    }
}
