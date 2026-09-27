//! The request pipeline.
//!
//! The pipeline is a fixed sequence of phases rather than a user-assembled middleware stack,
//! because the order is part of the security model (e.g. auth must run before rate limiting
//! keyed on the authenticated subject). Later phases slot in at the marked points:
//!
//! ```text
//! validate path → match route → [auth] → [rate limit] → [concurrency] →
//! pick endpoint → [breaker] → forward (per-try timeout) → [retry?] → response headers
//! ```
//!
//! Bodies are never buffered: the request body is handed to the upstream as a stream and the
//! upstream's response body is handed back as a stream, so hyper's flow control provides
//! backpressure end to end and memory per request is bounded by frame size, not body size.

pub mod error;
pub mod headers;

use std::{net::SocketAddr, time::Instant};

use http::{Request, Response, Version};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use tracing::{Instrument, Span, field, info, info_span, warn};

use self::{
    error::{ProxyError, error_chain},
    headers::{ForwardContext, RequestId, X_REQUEST_ID},
};
use crate::{
    body::{BoxError, ProxyBody},
    routing,
    snapshot::SharedSnapshot,
    upstream::HttpClient,
};

#[derive(Debug)]
pub struct Proxy {
    snapshot: SharedSnapshot,
    client: HttpClient,
}

impl Proxy {
    pub fn new(snapshot: SharedSnapshot, client: HttpClient) -> Self {
        Self { snapshot, client }
    }

    /// Handles one request. Infallible by design: every failure becomes a response.
    pub async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<ProxyBody> {
        let started = Instant::now();
        let request_id = RequestId::from_headers(req.headers());
        // PERF: span creation formats these fields for the log layer (an allocation or two per
        // request). Keep an eye on this in the Phase 9 profile.
        let span = info_span!(
            "request",
            request_id = request_id.as_str(),
            method = %req.method(),
            path = req.uri().path(),
            route = field::Empty,
            upstream = field::Empty,
        );

        async {
            let mut response = match self.forward(req, peer, &request_id).await {
                Ok(response) => response,
                Err(err) => {
                    if err.is_server_side() {
                        warn!(error = %error_chain(&err), "request failed");
                    }
                    err.into_response()
                }
            };
            response
                .headers_mut()
                .insert(&X_REQUEST_ID, request_id.header_value().clone());
            // Measured to response *headers*. Full-body duration needs a body wrapper that
            // observes end-of-stream; that arrives with metrics in Phase 7.
            info!(
                status = response.status().as_u16(),
                latency_ms = started.elapsed().as_secs_f64() * 1000.0,
                "request completed"
            );
            response
        }
        .instrument(span)
        .await
    }

    async fn forward(
        &self,
        req: Request<Incoming>,
        peer: SocketAddr,
        request_id: &RequestId,
    ) -> Result<Response<ProxyBody>, ProxyError> {
        if !routing::is_safe_path(req.uri().path()) {
            return Err(ProxyError::InvalidPath);
        }

        let host = headers::request_host(&req);
        let route = {
            // `load()` returns a guard that doesn't touch the snapshot's refcount (cheaper and
            // contention-free vs `load_full()`). We clone only the matched route's `Arc` and
            // drop the guard before any `.await` — guards are meant to be short-lived.
            let snapshot = self.snapshot.load();
            snapshot
                .router
                .lookup(
                    host.as_ref().and_then(|h| h.to_str().ok()),
                    req.uri().path(),
                )
                .cloned()
        }
        .ok_or(ProxyError::NoRoute)?;

        let span = Span::current();
        span.record("route", &*route.id);
        let endpoint = route.cluster.pick().ok_or(ProxyError::NoEndpoint)?;
        span.record("upstream", endpoint.url());

        let (mut parts, body) = req.into_parts();
        let uri = endpoint
            .uri_for(parts.uri.path_and_query())
            .map_err(|_| ProxyError::Internal("building upstream URI"))?;
        parts.uri = uri;
        // TODO(phase 2/6): negotiate HTTP/2 to upstreams that support it. Until then every
        // upstream request is HTTP/1.1 regardless of the client's protocol.
        parts.version = Version::HTTP_11;
        headers::prepare_upstream_headers(
            &mut parts.headers,
            &ForwardContext {
                peer_ip: peer.ip(),
                host: host.as_ref(),
                proto: "http",
                request_id,
            },
        );

        // The client's body stream is moved into the upstream request — not read, not copied.
        let upstream_req = Request::from_parts(parts, body.map_err(BoxError::from).boxed());
        let response = self
            .client
            .request(upstream_req)
            .await
            .map_err(ProxyError::from_client)?;

        let (mut parts, body) = response.into_parts();
        headers::remove_hop_by_hop(&mut parts.headers);
        Ok(Response::from_parts(
            parts,
            body.map_err(BoxError::from).boxed(),
        ))
    }
}
