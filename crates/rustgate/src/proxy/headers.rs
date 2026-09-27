//! Header handling at the proxy boundary.
//!
//! Hop-by-hop headers describe a single connection (RFC 9110 §7.6.1) and must not be
//! forwarded. This is correctness, not only hygiene: an HTTP/1.1 client would otherwise
//! receive the upstream's connection semantics (`Connection: close`, `Keep-Alive`, headers
//! named in `Connection`). HTTP/2 forbids connection-specific headers outright; hyper's
//! HTTP/2 server strips them defensively (with a warning), but the proxy shouldn't rely on
//! that, and the HTTP/1.1 side has no such safety net.

use std::net::IpAddr;

use http::{
    HeaderMap, HeaderName, HeaderValue, Request,
    header::{
        CONNECTION, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING,
        UPGRADE,
    },
};
use uuid::Uuid;

pub static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
pub static X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
pub static X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
pub static X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");

static HOP_BY_HOP: [HeaderName; 9] = [
    CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION,
    TE,
    TRAILER,
    TRANSFER_ENCODING,
    UPGRADE,
];

/// Removes the standard hop-by-hop headers plus any header named in `Connection`.
pub fn remove_hop_by_hop(headers: &mut HeaderMap) {
    // Collect first: we can't remove entries while borrowing the `Connection` values.
    // `collect` on an empty iterator doesn't allocate, so the common case is free.
    let listed: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in listed {
        headers.remove(name);
    }
    for name in &HOP_BY_HOP {
        headers.remove(name);
    }
}

pub struct ForwardContext<'a> {
    pub peer_ip: IpAddr,
    pub host: Option<&'a HeaderValue>,
    pub proto: &'static str,
    pub request_id: &'a RequestId,
}

pub fn prepare_upstream_headers(headers: &mut HeaderMap, ctx: &ForwardContext<'_>) {
    let wants_trailers = accepts_trailers(headers);
    // Must run first: otherwise a client could send `Connection: x-forwarded-for` and have
    // us strip a header we are about to add.
    remove_hop_by_hop(headers);
    if wants_trailers {
        // `TE` is hop-by-hop, but gRPC requires `te: trailers` end to end (and servers reject
        // requests without it). Like Envoy, forward only that token and drop the rest.
        headers.insert(TE, HeaderValue::from_static("trailers"));
    }
    append_forwarded_for(headers, ctx.peer_ip);
    headers.insert(&X_FORWARDED_PROTO, HeaderValue::from_static(ctx.proto));
    if let Some(host) = ctx.host {
        // Preserve the client's Host (HTTP/2 clients send `:authority` instead, so this also
        // translates it for HTTP/1.1 upstreams).
        headers.insert(HOST, host.clone());
        headers.insert(&X_FORWARDED_HOST, host.clone());
    }
    headers.insert(&X_REQUEST_ID, ctx.request_id.header_value().clone());
}

/// Whether the client's `TE` header lists the `trailers` token (possibly among others).
fn accepts_trailers(headers: &HeaderMap) -> bool {
    headers
        .get_all(TE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|token| {
            let name = token.split(';').next().unwrap_or_default().trim();
            name.eq_ignore_ascii_case("trailers")
        })
}

/// TODO(phase 3): only extend an inbound `X-Forwarded-For` when the peer is a configured
/// trusted proxy. Appending is already correct for upstreams that read the right-most entry,
/// but anything using the left-most entry (IP allow-lists, rate-limit keys) could be spoofed.
fn append_forwarded_for(headers: &mut HeaderMap, peer_ip: IpAddr) {
    let peer_ip = peer_ip.to_canonical();
    let mut existing = headers
        .get_all(&X_FORWARDED_FOR)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .peekable();
    let value = if existing.peek().is_some() {
        let mut joined = existing.collect::<Vec<_>>().join(", ");
        joined.push_str(", ");
        joined.push_str(&peer_ip.to_string());
        joined
    } else {
        peer_ip.to_string()
    };
    if let Ok(value) = HeaderValue::try_from(value) {
        headers.insert(&X_FORWARDED_FOR, value);
    }
}

/// The client-facing host: the URI authority (HTTP/2 `:authority`, or an absolute-form
/// HTTP/1.1 target) takes precedence over `Host`, per RFC 9112 §3.2.2.
pub fn request_host<B>(req: &Request<B>) -> Option<HeaderValue> {
    match req.uri().authority() {
        Some(authority) => {
            let without_userinfo = authority.as_str().rsplit('@').next().unwrap_or_default();
            HeaderValue::from_str(without_userinfo).ok()
        }
        None => req.headers().get(HOST).cloned(),
    }
}

/// A correlation ID, propagated to upstreams and echoed back to clients.
#[derive(Debug, Clone)]
pub struct RequestId(HeaderValue);

impl RequestId {
    /// Reuses a client-supplied `x-request-id` only if it is short and made of safe
    /// characters; anything else (log injection, oversized values) gets a fresh UUIDv7.
    /// v7 is time-ordered, which makes IDs roughly sortable in logs.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        headers
            .get(&X_REQUEST_ID)
            .filter(|v| is_valid_request_id(v.as_bytes()))
            .map_or_else(Self::generate, |v| Self(v.clone()))
    }

    fn generate() -> Self {
        // `TryFrom<String>` reuses the String's buffer instead of copying it.
        Self(
            HeaderValue::try_from(Uuid::now_v7().to_string())
                .expect("a UUID is a valid header value"),
        )
    }

    pub fn header_value(&self) -> &HeaderValue {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        // Validated (or generated) as ASCII, so this cannot fail.
        self.0.to_str().unwrap_or_default()
    }
}

fn is_valid_request_id(bytes: &[u8]) -> bool {
    (1..=128).contains(&bytes.len())
        && bytes
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_standard_and_connection_listed_headers() {
        let mut h = HeaderMap::new();
        h.insert(CONNECTION, HeaderValue::from_static("keep-alive, X-Secret"));
        h.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        h.insert("x-secret", HeaderValue::from_static("1"));
        h.insert(TE, HeaderValue::from_static("trailers"));
        h.insert(PROXY_AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        h.insert("x-keep", HeaderValue::from_static("1"));
        remove_hop_by_hop(&mut h);
        assert_eq!(h.len(), 1);
        assert!(h.contains_key("x-keep"));
    }

    #[test]
    fn client_cannot_strip_gateway_headers_via_connection() {
        let mut h = HeaderMap::new();
        h.insert(
            CONNECTION,
            HeaderValue::from_static("x-forwarded-for, x-request-id"),
        );
        let id = RequestId::generate();
        let ctx = ForwardContext {
            peer_ip: "10.0.0.1".parse().unwrap(),
            host: None,
            proto: "http",
            request_id: &id,
        };
        prepare_upstream_headers(&mut h, &ctx);
        assert_eq!(h[&X_FORWARDED_FOR], "10.0.0.1");
        assert_eq!(h[&X_REQUEST_ID], id.header_value());
    }

    #[test]
    fn forwards_only_the_trailers_token_of_te() {
        let id = RequestId::generate();
        let ctx = ForwardContext {
            peer_ip: "10.0.0.1".parse().unwrap(),
            host: None,
            proto: "http",
            request_id: &id,
        };
        let mut h = HeaderMap::new();
        h.insert(TE, HeaderValue::from_static("gzip;q=0.5, Trailers"));
        prepare_upstream_headers(&mut h, &ctx);
        assert_eq!(h[TE], "trailers");

        let mut h = HeaderMap::new();
        h.insert(TE, HeaderValue::from_static("gzip"));
        prepare_upstream_headers(&mut h, &ctx);
        assert!(!h.contains_key(TE));
    }

    #[test]
    fn appends_to_existing_forwarded_for() {
        let mut h = HeaderMap::new();
        h.append(&X_FORWARDED_FOR, HeaderValue::from_static("1.1.1.1"));
        h.append(&X_FORWARDED_FOR, HeaderValue::from_static("2.2.2.2"));
        append_forwarded_for(&mut h, "::ffff:10.0.0.1".parse().unwrap());
        assert_eq!(h[&X_FORWARDED_FOR], "1.1.1.1, 2.2.2.2, 10.0.0.1");
    }

    #[test]
    fn validates_request_ids() {
        assert!(is_valid_request_id(b"abc-123_x.y:z"));
        assert!(!is_valid_request_id(b""));
        assert!(!is_valid_request_id(b"has space"));
        assert!(!is_valid_request_id(b"line\nbreak"));
        assert!(!is_valid_request_id(&[b'a'; 129]));
    }
}
