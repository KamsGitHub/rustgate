//! A configurable backend for demos and tests. One binary plays users/orders/analytics by
//! name. Any path echoes the request as JSON; query knobs inject behaviour:
//!
//! - `?delay_ms=250`  — sleep before responding (capped at 60s)
//! - `?status=503`    — respond with this status
//!
//! Crash / connection-refusal scenarios come from stopping the process or container.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
};
use clap::Parser;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Parser)]
struct Args {
    /// Instance name reported in responses, e.g. `users-1`.
    #[arg(long, env = "DEMO_NAME", default_value = "demo")]
    name: String,
    #[arg(long, env = "DEMO_BIND", default_value = "127.0.0.1:9001")]
    bind: SocketAddr,
}

#[derive(Debug, Deserialize)]
struct Knobs {
    delay_ms: Option<u64>,
    status: Option<u16>,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt().init();
    let args = Args::parse();

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .fallback(echo)
        .with_state(Arc::<str>::from(args.name.as_str()));

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(name = %args.name, addr = %args.bind, "demo-service listening");
    axum::serve(listener, app).await
}

async fn echo(
    State(name): State<Arc<str>>,
    Query(knobs): Query<Knobs>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    if let Some(ms) = knobs.delay_ms {
        tokio::time::sleep(Duration::from_millis(ms.min(60_000))).await;
    }
    let status = knobs
        .status
        .and_then(|s| StatusCode::from_u16(s).ok())
        .unwrap_or(StatusCode::OK);
    let headers: BTreeMap<&str, &str> = headers
        .iter()
        .filter_map(|(k, v)| Some((k.as_str(), v.to_str().ok()?)))
        .collect();
    let body = json!({
        "service": &*name,
        "method": method.as_str(),
        "path": uri.path(),
        "query": uri.query(),
        "headers": headers,
    });
    (status, Json(body)).into_response()
}
