//! The `/healthz` HTTP server.
//!
//! `GET /healthz` returns `200` and a JSON body with the crate name, crate version and git
//! sha. The sha comes from the `SM_GIT_SHA` environment variable at build time, or `unknown`
//! when it is not set.

use std::net::SocketAddr;

use anyhow::Context as _;
use axum::{Json, Router, routing::get};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Git sha from `SM_GIT_SHA` at build time, or `"unknown"`.
pub const GIT_SHA: &str = match option_env!("SM_GIT_SHA") {
    Some(sha) => sha,
    None => "unknown",
};

/// Build information returned by `/healthz`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildInfo {
    /// Crate name.
    pub name: &'static str,
    /// Crate version.
    pub version: &'static str,
    /// Git sha, or `"unknown"`.
    pub git_sha: &'static str,
}

/// Returns the build information for this crate.
pub fn build_info() -> BuildInfo {
    BuildInfo {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        git_sha: GIT_SHA,
    }
}

/// A running `/healthz` server.
///
/// Call [`ServerHandle::shutdown`] to stop it and wait for it to finish. Dropping the handle
/// also stops the server, but does not wait for it.
#[derive(Debug)]
pub struct ServerHandle {
    local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl ServerHandle {
    /// The address the server is bound to. Useful when bound to port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops the server gracefully and waits until it has finished.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        if let Some(tx) = self.shutdown.take() {
            // The receiver is only gone if the server already stopped, which is fine.
            let _ = tx.send(());
        }
        self.task
            .await
            .context("healthz server task panicked or was cancelled")?
            .context("healthz server failed")?;
        Ok(())
    }
}

/// Binds `addr` and serves `GET /healthz` in a background task.
///
/// Pass port 0 to let the OS pick a port, then read it from [`ServerHandle::local_addr`].
pub async fn serve(addr: SocketAddr) -> anyhow::Result<ServerHandle> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding healthz to {addr}"))?;
    let local_addr = listener
        .local_addr()
        .context("reading healthz local address")?;

    let (tx, rx) = oneshot::channel::<()>();
    let app = Router::new().route("/healthz", get(healthz));
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                // Resolves on an explicit shutdown, and also when the handle is dropped.
                let _ = rx.await;
            })
            .await
    });

    tracing::info!(%local_addr, "healthz listening");
    Ok(ServerHandle {
        local_addr,
        shutdown: Some(tx),
        task,
    })
}

async fn healthz() -> Json<BuildInfo> {
    Json(build_info())
}
