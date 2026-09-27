//! RustGate: an API gateway / reverse proxy.
//!
//! The crate is a library plus a thin binary so integration tests can run a real gateway
//! in-process on ephemeral ports.

pub mod admin;
pub mod body;
pub mod config;
pub mod proxy;
pub mod routing;
pub mod server;
pub mod snapshot;
pub mod telemetry;
pub mod upstream;

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use tokio::{net::TcpListener, task::JoinSet};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{Config, ConfigError},
    proxy::Proxy,
    snapshot::{SharedSnapshot, Snapshot},
    upstream::EndpointRegistry,
};

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("binding {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },
}

/// A running gateway. Stop it with [`Gateway::shutdown`]; dropping it aborts the listeners
/// without draining.
#[derive(Debug)]
pub struct Gateway {
    data_addr: SocketAddr,
    admin_addr: SocketAddr,
    tasks: JoinSet<()>,
    shutdown: CancellationToken,
}

/// Returned by [`Gateway::shutdown`] when connections were still open at the deadline.
#[derive(Debug, thiserror::Error)]
#[error("drain deadline exceeded; remaining connections were aborted")]
pub struct DrainTimeout;

impl Gateway {
    /// Validates the config, binds both listeners and starts serving. Binding happens before
    /// returning, so the addresses are immediately connectable (and port 0 works in tests).
    pub async fn start(config: &Config) -> Result<Self, StartError> {
        // The registry outlives snapshots: hot reload (Phase 8) compiles new snapshots against
        // the same registry, so per-endpoint state survives a config change.
        let registry = EndpointRegistry::default();
        let snapshot: SharedSnapshot = Arc::new(ArcSwap::from_pointee(Snapshot::compile(
            config, 1, &registry,
        )?));

        let data = bind(config.listeners.data.bind).await?;
        let admin = bind(config.listeners.admin.bind).await?;
        let data_addr = local_addr(&data, config.listeners.data.bind)?;
        let admin_addr = local_addr(&admin, config.listeners.admin.bind)?;

        let proxy = Arc::new(Proxy::new(Arc::clone(&snapshot), upstream::build_client()));
        let shutdown = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(server::serve(data, proxy, shutdown.clone()));
        tasks.spawn(admin::serve(admin, snapshot, shutdown.clone()));

        Ok(Self {
            data_addr,
            admin_addr,
            tasks,
            shutdown,
        })
    }

    pub fn data_addr(&self) -> SocketAddr {
        self.data_addr
    }

    pub fn admin_addr(&self) -> SocketAddr {
        self.admin_addr
    }

    /// Resolves if any gateway task exits, which only happens on a fatal error.
    pub async fn wait(&mut self) {
        self.tasks.join_next().await;
    }

    /// Stops accepting connections, lets in-flight requests finish, and returns once every
    /// connection has closed. Connections still open after `deadline` are aborted.
    ///
    /// TODO(phase 2): flip admin readiness to failing and wait a configurable delay before
    /// closing the listener, so load balancers stop sending traffic first.
    pub async fn shutdown(mut self, deadline: Duration) -> Result<(), DrainTimeout> {
        self.shutdown.cancel();
        let drained = tokio::time::timeout(deadline, async {
            while self.tasks.join_next().await.is_some() {}
        })
        .await;
        // On timeout, dropping `self` aborts the listener tasks. Connection tasks are detached;
        // they end when their peer disconnects or their own timeouts fire.
        drained.map_err(|_| DrainTimeout)
    }
}

async fn bind(addr: SocketAddr) -> Result<TcpListener, StartError> {
    TcpListener::bind(addr)
        .await
        .map_err(|source| StartError::Bind { addr, source })
}

fn local_addr(listener: &TcpListener, addr: SocketAddr) -> Result<SocketAddr, StartError> {
    listener
        .local_addr()
        .map_err(|source| StartError::Bind { addr, source })
}
