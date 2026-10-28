//! Previews: a sandbox port exposed on a hub-signed URL (PLAN-v2 §4.3).
//!
//! - `local`: [`NoPreview`]. Local runners expose no ports.
//! - `sandbox`: [`WorkspacePreview`]. The Workspace DO exposes the port through an injected
//!   [`PreviewClient`]. The default client is [`UnwiredPreviewClient`] until M1A-011.

use std::sync::Arc;

use crate::host::BoxFuture;

/// Preview errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreviewError {
    /// Local mode has no previews.
    #[error("no previews in local mode: the Workspace DO exposes ports in sandbox mode")]
    NotAvailable,
    /// The client is not wired yet.
    #[error("preview {op} is not implemented yet: {item} fills it")]
    NotImplemented {
        op: &'static str,
        item: &'static str,
    },
    /// The Workspace DO rejected or failed the request.
    #[error("preview failed: {0}")]
    Client(String),
}

/// Exposes a workspace port and returns its URL.
pub trait PreviewProvider: Send + Sync {
    /// Exposes `port` and returns the URL where it can be reached.
    fn expose(&self, port: u16) -> BoxFuture<'_, Result<String, PreviewError>>;
}

/// The Workspace DO's preview endpoint. Injected into [`WorkspacePreview`].
pub trait PreviewClient: Send + Sync {
    /// Asks the Workspace DO to expose `port` and returns the signed URL.
    fn expose(&self, port: u16) -> BoxFuture<'_, Result<String, PreviewError>>;
}

/// The `local` preview. It exposes nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoPreview;

impl PreviewProvider for NoPreview {
    fn expose(&self, _port: u16) -> BoxFuture<'_, Result<String, PreviewError>> {
        Box::pin(std::future::ready(Err(PreviewError::NotAvailable)))
    }
}

/// The `sandbox` preview: delegates to a [`PreviewClient`].
pub struct WorkspacePreview {
    client: Arc<dyn PreviewClient>,
}

impl WorkspacePreview {
    /// Builds the preview with `client`.
    pub fn new(client: Arc<dyn PreviewClient>) -> Self {
        Self { client }
    }
}

impl PreviewProvider for WorkspacePreview {
    fn expose(&self, port: u16) -> BoxFuture<'_, Result<String, PreviewError>> {
        self.client.expose(port)
    }
}

/// The preview client before M1A-011 wires the Workspace DO. Every request returns
/// `NotImplemented`.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnwiredPreviewClient;

impl PreviewClient for UnwiredPreviewClient {
    fn expose(&self, _port: u16) -> BoxFuture<'_, Result<String, PreviewError>> {
        Box::pin(std::future::ready(Err(PreviewError::NotImplemented {
            op: "expose",
            item: "M1A-011",
        })))
    }
}
