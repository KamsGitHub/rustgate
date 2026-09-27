//! Gateway-generated failures and how they map to HTTP responses.
//!
//! Every error response has a stable machine-readable `error` code, so clients and dashboards
//! can tell "the gateway rejected this" (400/404/503) apart from "the upstream failed" (502).

use http::{Response, StatusCode, header::CONTENT_TYPE};
use hyper_util::client::legacy;

use crate::body::{ProxyBody, full};

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("request path contains '.' or '..' segments")]
    InvalidPath,
    #[error("no route matches this request")]
    NoRoute,
    #[error("no upstream endpoint available")]
    NoEndpoint,
    #[error("could not connect to upstream")]
    UpstreamConnect(#[source] legacy::Error),
    #[error("upstream request failed")]
    Upstream(#[source] legacy::Error),
    #[error("internal gateway error: {0}")]
    Internal(&'static str),
}

impl ProxyError {
    pub fn from_client(err: legacy::Error) -> Self {
        if err.is_connect() {
            Self::UpstreamConnect(err)
        } else {
            Self::Upstream(err)
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::InvalidPath => StatusCode::BAD_REQUEST,
            Self::NoRoute => StatusCode::NOT_FOUND,
            Self::NoEndpoint => StatusCode::SERVICE_UNAVAILABLE,
            Self::UpstreamConnect(_) | Self::Upstream(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPath => "invalid_path",
            Self::NoRoute => "no_route",
            Self::NoEndpoint => "no_endpoint",
            Self::UpstreamConnect(_) => "upstream_connect_failed",
            Self::Upstream(_) => "upstream_error",
            Self::Internal(_) => "internal",
        }
    }

    /// Whether this is worth a warning in the logs (upstream/gateway trouble) as opposed to
    /// an ordinary client mistake that the access log already records.
    pub fn is_server_side(&self) -> bool {
        self.status().is_server_error()
    }

    pub fn into_response(self) -> Response<ProxyBody> {
        let body = serde_json::json!({ "error": self.code(), "message": self.to_string() });
        let mut response = Response::new(full(body.to_string()));
        *response.status_mut() = self.status();
        response.headers_mut().insert(
            CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        response
    }
}

/// Formats an error with its full `source()` chain, e.g.
/// `could not connect to upstream: client error (Connect): tcp connect error: Connection refused`.
pub fn error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        out.push_str(": ");
        out.push_str(&e.to_string());
        source = e.source();
    }
    out
}
