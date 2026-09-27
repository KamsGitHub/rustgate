//! Data-plane listener: accepts TCP connections and serves HTTP/1.1 and HTTP/2 on each.
//!
//! Concurrency model: one Tokio task per connection. hyper drives the connection inside that
//! task (and spawns a task per stream for HTTP/2). There is no shared request queue; the
//! kernel's accept backlog is the only queue before a request reaches the pipeline.
//!
//! Shutdown: when the token is cancelled the accept loop stops and every open connection is
//! told to finish gracefully (HTTP/1.1 closes after the in-flight response, HTTP/2 sends
//! GOAWAY). `serve` returns once all connections are gone; the caller owns the deadline.

use std::{convert::Infallible, sync::Arc, time::Duration};

use hyper::service::service_fn;
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::{conn::auto, graceful::GracefulShutdown},
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::proxy::Proxy;

/// Slowloris protection: a client must finish sending request headers within this time.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn serve(listener: TcpListener, proxy: Arc<Proxy>, shutdown: CancellationToken) {
    // `auto` sniffs the HTTP/2 connection preface, so one port serves both protocols
    // (h2c prior-knowledge today; ALPN once TLS lands).
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);

    let graceful = GracefulShutdown::new();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = shutdown.cancelled() => break,
        };
        let (stream, peer) = match accepted {
            Ok(conn) => conn,
            Err(e) => {
                // Usually EMFILE/ENFILE (fd exhaustion). Exiting would turn a load spike into
                // an outage, and retrying immediately would spin the CPU; back off briefly.
                warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        if let Err(e) = stream.set_nodelay(true) {
            debug!(error = %e, "failed to set TCP_NODELAY");
        }

        let proxy = Arc::clone(&proxy);
        let builder = builder.clone();
        let watcher = graceful.watcher();
        tokio::spawn(async move {
            // The service must return a `'static` future, so each request takes its own
            // `Arc<Proxy>` clone. PERF: that is an atomic increment on one shared cache line
            // per request; a candidate for the Phase 9 profile.
            let service = service_fn(move |req| {
                let proxy = Arc::clone(&proxy);
                async move { Ok::<_, Infallible>(proxy.handle(req, peer).await) }
            });
            let conn = builder.serve_connection(TokioIo::new(stream), service);
            if let Err(e) = watcher.watch(conn).await {
                debug!(%peer, error = %e, "connection ended with error");
            }
        });
    }

    // Dropping the listener here closes the socket, so new connections are refused while
    // existing ones drain.
    drop(listener);
    info!(
        connections = graceful.count(),
        "draining data-plane connections"
    );
    graceful.shutdown().await;
}
