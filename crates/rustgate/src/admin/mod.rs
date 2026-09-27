//! Control-plane admin API, served on its own listener (localhost by default) so it is never
//! reachable through the data-plane port. axum is a good fit here: a fixed set of routes,
//! JSON in and out, and nothing latency-critical.

use axum::{Json, Router, extract::State, routing::get};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::error;

use crate::snapshot::SharedSnapshot;

pub fn router(snapshot: SharedSnapshot) -> Router {
    Router::new()
        .route("/admin/health", get(health))
        .with_state(snapshot)
}

pub async fn serve(listener: TcpListener, snapshot: SharedSnapshot, shutdown: CancellationToken) {
    let server =
        axum::serve(listener, router(snapshot)).with_graceful_shutdown(shutdown.cancelled_owned());
    if let Err(e) = server.await {
        error!(error = %e, "admin server stopped");
    }
}

async fn health(State(snapshot): State<SharedSnapshot>) -> Json<Value> {
    let snapshot = snapshot.load();
    Json(json!({
        "status": "ok",
        "config_version": snapshot.version,
        "routes": snapshot.routes.len(),
        "clusters": snapshot.clusters.len(),
    }))
}
