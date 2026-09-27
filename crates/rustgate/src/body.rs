//! The single body type used on both sides of the proxy.
//!
//! Requests arrive as `hyper::body::Incoming`, responses from upstream are also `Incoming`,
//! and gateway-generated responses (errors) are `Full<Bytes>`. Boxing lets one `Response`
//! type carry any of them without making every function generic.
//!
//! PERF: boxing costs one heap allocation per body and a virtual call per frame. That is
//! noise next to a network round trip, but it is on the hot path, so it is a candidate to
//! revisit with a profile (Phase 9), e.g. with an enum body instead of a trait object.
//!
//! Data frames are `Bytes`: reference-counted slices, so a chunk read from the client is
//! handed to the upstream connection without copying.

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub type ProxyBody = BoxBody<Bytes, BoxError>;

pub fn full(bytes: impl Into<Bytes>) -> ProxyBody {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

pub fn empty() -> ProxyBody {
    Empty::new().map_err(|never| match never {}).boxed()
}
