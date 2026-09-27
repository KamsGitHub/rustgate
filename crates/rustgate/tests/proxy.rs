//! End-to-end tests: a real gateway on ephemeral ports in front of in-process upstreams,
//! driven by a real HTTP client. These are the Phase 1 exit criteria: routing, streaming,
//! hop-by-hop header removal, error mapping and graceful drain.

// `allow-unwrap-in-tests` only covers `#[test]` fns; in the helpers a panic is the report too.
#![allow(clippy::unwrap_used)]

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, Uri, Version, header::CONNECTION},
    response::{IntoResponse, Response},
    routing,
};
use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use rustgate::{
    Gateway,
    body::{BoxError, ProxyBody, empty, full},
    config::Config,
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Notify, time::timeout};

/// Upper bound for any single step, so a regression fails the test instead of hanging it.
const STEP: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------------------
// Upstream test double
// ---------------------------------------------------------------------------------------

#[derive(Clone)]
struct UpstreamState {
    name: &'static str,
    /// Holds back the second chunk of `/stream` until the test releases it.
    gate: Arc<Notify>,
}

struct Upstream {
    addr: SocketAddr,
    gate: Arc<Notify>,
}

/// Any path echoes the request as JSON (`?delay_ms=N` sleeps first); `/stream` sends one
/// chunk, then waits for `gate` before sending the second.
async fn upstream(name: &'static str) -> Upstream {
    let gate = Arc::new(Notify::new());
    let app = axum::Router::new()
        .route("/stream", routing::get(stream))
        .fallback(echo)
        .with_state(UpstreamState {
            name,
            gate: Arc::clone(&gate),
        });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Upstream { addr, gate }
}

async fn echo(
    State(state): State<UpstreamState>,
    method: Method,
    version: Version,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Some(ms) = query_param(&uri, "delay_ms") {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    let body_len = body.collect().await.unwrap().to_bytes().len();
    let headers: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), json!(v.to_str().unwrap_or("<binary>"))))
        .collect();
    let mut response = Json(json!({
        "upstream": state.name,
        "method": method.as_str(),
        "version": format!("{version:?}"),
        "uri": uri.to_string(),
        "headers": headers,
        "body_len": body_len,
    }))
    .into_response();
    // A connection-scoped header the gateway must not pass on to its client.
    let headers = response.headers_mut();
    headers.insert("x-upstream-secret", HeaderValue::from_static("1"));
    headers.insert(CONNECTION, HeaderValue::from_static("x-upstream-secret"));
    response
}

async fn stream(State(state): State<UpstreamState>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(2);
    tokio::spawn(async move {
        tx.send(Ok(Bytes::from_static(b"first;"))).await.unwrap();
        state.gate.notified().await;
        tx.send(Ok(Bytes::from_static(b"second"))).await.unwrap();
    });
    Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)).into_response()
}

fn query_param(uri: &Uri, key: &str) -> Option<u64> {
    uri.query()?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse().ok())
}

// ---------------------------------------------------------------------------------------
// Gateway and client helpers
// ---------------------------------------------------------------------------------------

/// The standard topology: `users` and `orders` clusters, prefix routes for both, `/stream`
/// on `users`, and a host route sending everything on `orders.localhost` to `orders`.
async fn start_gateway(users: SocketAddr, orders: SocketAddr) -> Gateway {
    start(&format!(
        r#"
listeners:
  data: {{ bind: "127.0.0.1:0" }}
  admin: {{ bind: "127.0.0.1:0" }}
clusters:
  users: {{ endpoints: ["http://{users}"] }}
  orders: {{ endpoints: ["http://{orders}"] }}
routes:
  - {{ id: users-api, match: {{ path_prefix: /users }}, cluster: users }}
  - {{ id: stream, match: {{ path_prefix: /stream }}, cluster: users }}
  - {{ id: orders-api, match: {{ path_prefix: /orders }}, cluster: orders }}
  - {{ id: orders-host, match: {{ host: orders.localhost, path_prefix: / }}, cluster: orders }}
"#
    ))
    .await
}

async fn start(yaml: &str) -> Gateway {
    Gateway::start(&Config::from_yaml(yaml).unwrap())
        .await
        .unwrap()
}

type TestClient = Client<HttpConnector, ProxyBody>;

fn http1_client() -> TestClient {
    Client::builder(TokioExecutor::new()).build_http()
}

fn get(addr: SocketAddr, path: &str) -> http::request::Builder {
    Request::get(format!("http://{addr}{path}"))
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    json: Value,
}

async fn send(client: &TestClient, req: Request<ProxyBody>) -> Reply {
    let response = timeout(STEP, client.request(req)).await.unwrap().unwrap();
    let (parts, body) = response.into_parts();
    let bytes = timeout(STEP, body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn routes_by_host_and_longest_prefix() {
    let (users, orders) = (upstream("users").await, upstream("orders").await);
    let gw = start_gateway(users.addr, orders.addr).await;
    let client = http1_client();
    let addr = gw.data_addr();

    let r = send(&client, get(addr, "/users/42?x=1").body(empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["upstream"], "users");
    assert_eq!(
        r.json["uri"], "/users/42?x=1",
        "path and query are forwarded as-is"
    );

    let r = send(&client, get(addr, "/orders").body(empty()).unwrap()).await;
    assert_eq!(r.json["upstream"], "orders");

    // Prefixes are segment-aligned.
    let r = send(&client, get(addr, "/usersx").body(empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json["error"], "no_route");

    // A host with its own routes uses only those, even for a path another route matches.
    let req = get(addr, "/users/1")
        .header("host", "orders.localhost:8080")
        .body(empty())
        .unwrap();
    let r = send(&client, req).await;
    assert_eq!(r.json["upstream"], "orders");
    assert_eq!(r.json["headers"]["host"], "orders.localhost:8080");

    let r = send(
        &client,
        get(addr, "/users/../orders").body(empty()).unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json["error"], "invalid_path");
}

#[tokio::test]
async fn forwards_context_headers_and_strips_hop_by_hop() {
    let (users, orders) = (upstream("users").await, upstream("orders").await);
    let gw = start_gateway(users.addr, orders.addr).await;
    let client = http1_client();

    let req = get(gw.data_addr(), "/users")
        .header("connection", "x-client-secret")
        .header("x-client-secret", "1")
        .header("keep-alive", "timeout=5")
        .header("proxy-authorization", "Basic c2VjcmV0")
        .header("x-forwarded-for", "203.0.113.9")
        .header("x-request-id", "abc-123")
        .body(empty())
        .unwrap();
    let r = send(&client, req).await;
    let seen = &r.json["headers"];

    // Request direction: hop-by-hop headers, including ones named in `Connection`, are gone.
    for name in ["x-client-secret", "keep-alive", "proxy-authorization"] {
        assert!(seen.get(name).is_none(), "{name} leaked to upstream");
    }
    assert_eq!(seen["x-forwarded-for"], "203.0.113.9, 127.0.0.1");
    assert_eq!(seen["x-forwarded-proto"], "http");
    assert_eq!(seen["x-forwarded-host"], gw.data_addr().to_string());
    assert_eq!(seen["x-request-id"], "abc-123");

    // Response direction.
    assert!(r.headers.get("x-upstream-secret").is_none());
    assert_eq!(r.headers["x-request-id"], "abc-123");

    // Without a client-supplied ID, the gateway generates one and uses it on both sides.
    let r = send(
        &client,
        get(gw.data_addr(), "/users").body(empty()).unwrap(),
    )
    .await;
    let generated = r.headers["x-request-id"].to_str().unwrap();
    assert_eq!(r.json["headers"]["x-request-id"], generated);
}

#[tokio::test]
async fn streams_response_without_buffering() {
    let (users, orders) = (upstream("users").await, upstream("orders").await);
    let gw = start_gateway(users.addr, orders.addr).await;
    let client = http1_client();

    let req = get(gw.data_addr(), "/stream").body(empty()).unwrap();
    let response = timeout(STEP, client.request(req)).await.unwrap().unwrap();
    let mut body = response.into_body();

    // The upstream has not finished its body. If the gateway buffered, this would time out.
    let first = timeout(STEP, body.frame()).await.unwrap().unwrap().unwrap();
    assert_eq!(first.into_data().unwrap(), "first;");

    users.gate.notify_one();
    let rest = timeout(STEP, body.collect()).await.unwrap().unwrap();
    assert_eq!(rest.to_bytes(), "second");
}

#[tokio::test]
async fn streams_large_chunked_request_body() {
    let (users, orders) = (upstream("users").await, upstream("orders").await);
    let gw = start_gateway(users.addr, orders.addr).await;
    let client = http1_client();

    // 8 MiB with no Content-Length, so the body is sent (and must be forwarded) chunked.
    let chunks =
        (0..128).map(|_| Ok::<_, BoxError>(Frame::data(Bytes::from(vec![7u8; 64 * 1024]))));
    let body = StreamBody::new(tokio_stream::iter(chunks)).boxed();
    let req = Request::post(format!("http://{}/users/upload", gw.data_addr()))
        .body(body)
        .unwrap();
    let r = send(&client, req).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["method"], "POST");
    assert_eq!(r.json["body_len"], 8 * 1024 * 1024);
}

#[tokio::test]
async fn returns_502_when_upstream_refuses_connection() {
    // Reserve a port, then free it: nothing listens there.
    let dead = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let gw = start(&format!(
        r#"
listeners:
  data: {{ bind: "127.0.0.1:0" }}
  admin: {{ bind: "127.0.0.1:0" }}
clusters:
  dead: {{ endpoints: ["http://{dead}"] }}
routes:
  - {{ id: dead, match: {{ path_prefix: / }}, cluster: dead }}
"#
    ))
    .await;

    let r = send(
        &http1_client(),
        get(gw.data_addr(), "/x").body(empty()).unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY);
    assert_eq!(r.json["error"], "upstream_connect_failed");
    assert_eq!(r.headers["content-type"], "application/json");
    assert!(r.headers.contains_key("x-request-id"));
}

#[tokio::test]
async fn serves_http2_clients_over_http1_upstreams() {
    let (users, orders) = (upstream("users").await, upstream("orders").await);
    let gw = start_gateway(users.addr, orders.addr).await;
    // h2c with prior knowledge: the same port serves both protocols.
    let client: TestClient = Client::builder(TokioExecutor::new())
        .http2_only(true)
        .build_http();

    let req = get(gw.data_addr(), "/users/1").body(full("hello")).unwrap();
    let r = send(&client, req).await;
    // HTTP/2 client in, HTTP/1.1 upstream out: `:authority` becomes `Host`.
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["version"], "HTTP/1.1");
    assert_eq!(r.json["body_len"], 5);
    assert_eq!(r.json["headers"]["host"], gw.data_addr().to_string());
    assert!(r.headers.get("x-upstream-secret").is_none());
}

#[tokio::test]
async fn shutdown_drains_in_flight_requests() {
    let (users, orders) = (upstream("users").await, upstream("orders").await);
    let gw = start_gateway(users.addr, orders.addr).await;
    let addr = gw.data_addr();

    let in_flight = tokio::spawn(async move {
        let client = http1_client();
        send(
            &client,
            get(addr, "/users?delay_ms=500").body(empty()).unwrap(),
        )
        .await
    });
    // Let the request reach the upstream before shutting down.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let started = Instant::now();
    timeout(STEP, gw.shutdown(STEP)).await.unwrap().unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "shutdown returned before the in-flight request finished"
    );

    let r = timeout(STEP, in_flight).await.unwrap().unwrap();
    assert_eq!(r.status, StatusCode::OK, "in-flight request was dropped");
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err(),
        "listener still accepting after shutdown"
    );
}

#[tokio::test]
async fn shutdown_gives_up_at_the_deadline() {
    let (users, orders) = (upstream("users").await, upstream("orders").await);
    let gw = start_gateway(users.addr, orders.addr).await;
    let addr = gw.data_addr();

    let _stuck = tokio::spawn(async move {
        let client = http1_client();
        let _ = client
            .request(get(addr, "/users?delay_ms=10000").body(empty()).unwrap())
            .await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let result = timeout(STEP, gw.shutdown(Duration::from_millis(100)))
        .await
        .unwrap();
    assert!(result.is_err());
}

#[tokio::test]
async fn admin_health_reports_loaded_config() {
    let (users, orders) = (upstream("users").await, upstream("orders").await);
    let gw = start_gateway(users.addr, orders.addr).await;

    let r = send(
        &http1_client(),
        get(gw.admin_addr(), "/admin/health").body(empty()).unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.json,
        json!({ "status": "ok", "config_version": 1, "routes": 4, "clusters": 2 })
    );
}
