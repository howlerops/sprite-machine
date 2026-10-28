//! Blob storage behind a trait (plan §4.2 "R2"): R2 (`BLOBS`) in the Worker, [`MemoryBlobs`]
//! natively. Keys are plain strings; the service decides them (org-scoped, see `service`).

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;

/// A failed R2 call. Carries the backend's message; never a URL or a key's secret part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobError(pub String);

/// One stored object: its bytes and the content type it was written with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

/// Async access to one bucket. A `put` replaces the object at `key`.
pub trait Blobs {
    fn put(
        &self,
        key: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> impl Future<Output = Result<(), BlobError>>;

    /// `None` when no object is stored at `key`.
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Stored>, BlobError>>;
}

/// The in-memory fake for native tests: the same semantics as the bucket.
#[derive(Debug, Default)]
pub struct MemoryBlobs {
    objects: Mutex<HashMap<String, Stored>>,
}

impl MemoryBlobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every stored key, sorted.
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.lock().keys().cloned().collect();
        keys.sort();
        keys
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Stored>> {
        self.objects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Blobs for MemoryBlobs {
    async fn put(&self, key: &str, bytes: Vec<u8>, content_type: &str) -> Result<(), BlobError> {
        let stored = Stored {
            bytes,
            content_type: content_type.to_owned(),
        };
        self.lock().insert(key.to_owned(), stored);
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Stored>, BlobError> {
        Ok(self.lock().get(key).cloned())
    }
}
