//! Attachments, the part that needs no window (plan §3, surface 1; client-api.md §5.6).
//!
//! - [`AttachmentSet`] is the model: pending files with a name, size and state, the checks that
//!   refuse a file before anything is uploaded ([`check`]), and the gate that decides whether a
//!   send may go ([`Gate`]).
//! - [`Uploader`] is the injectable upload step. [`HttpUploader`] is the real one: `POST
//!   {api}/attachments`, then `PUT` the bytes to the signed `upload_url`, and the answer is the
//!   `{type:"file"}` part the prompt carries. Tests use a fake.
//!
//! Limits follow the hub (`crates/hub/src/attachments`): 1 byte to 20 MB, a name of at most 255
//! characters, a `type/subtype` MIME type. The hub accepts any type; the client additionally
//! refuses programs (see [`refused_type`]) because an agent has no use for them and they are the
//! likeliest paste or drop mistake.
//!
//! Moving this into `client-core` (so the mobile web page and the CLI share it) is listed as
//! discovered work in the M3-006 report.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use protocol::tunnel::ContentPart;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{tr, tr_args};

/// The largest attachment the hub takes, in bytes (client-api.md §5.6: 20 MB).
pub const MAX_BYTES: u64 = 20 * 1024 * 1024;

/// Longest name the hub accepts, in characters.
pub const MAX_NAME_CHARS: usize = 255;

/// Extensions of programs and installers.
const REFUSED_EXTENSIONS: [&str; 10] = [
    "exe", "dll", "msi", "scr", "com", "bat", "cmd", "dmg", "pkg", "iso",
];

/// MIME types of programs.
const REFUSED_MIMES: [&str; 6] = [
    "application/x-msdownload",
    "application/x-dosexec",
    "application/x-msdos-program",
    "application/x-executable",
    "application/x-mach-binary",
    "application/vnd.microsoft.portable-executable",
];

/// Why a file was not attached. Shown as a Fluent message ([`Reject::message`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    Empty { name: String },
    TooLarge { name: String, size: u64 },
    Type { name: String, mime: String },
    Unreadable { name: String, detail: String },
}

impl Reject {
    /// The sentence naming the file and the reason.
    pub fn message(&self) -> String {
        match self {
            Self::Empty { name } => tr_args("compose-attach-rejected-empty", &[("name", name)]),
            Self::TooLarge { name, size } => tr_args(
                "compose-attach-rejected-too-large",
                &[
                    ("name", name),
                    ("size", &format_size(*size)),
                    ("limit", &format_size(MAX_BYTES)),
                ],
            ),
            Self::Type { name, mime } => tr_args(
                "compose-attach-rejected-type",
                &[("name", name), ("mime", mime)],
            ),
            Self::Unreadable { name, detail } => tr_args(
                "compose-attach-rejected-unreadable",
                &[("name", name), ("detail", detail)],
            ),
        }
    }
}

/// `1.5 KB`, `20.0 MB`: binary units, one decimal from a kilobyte up.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if value < KB * KB {
        format!("{:.1} KB", value / KB)
    } else if value < KB * KB * KB {
        format!("{:.1} MB", value / (KB * KB))
    } else {
        format!("{:.1} GB", value / (KB * KB * KB))
    }
}

/// The MIME type to declare for a file called `name`: from its extension, else
/// `application/octet-stream`.
pub fn guess_mime(name: &str) -> String {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "tif" | "tiff" => "image/tiff",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "zip" => "application/zip",
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "xml" => "text/xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "c" | "h" | "cpp" | "java" | "sh"
        | "rb" | "swift" | "kt" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_owned()
}

/// `true` for a program or installer, by extension or MIME type.
pub fn refused_type(name: &str, mime: &str) -> bool {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    REFUSED_EXTENSIONS.contains(&extension.as_str())
        || REFUSED_MIMES.contains(&mime.to_ascii_lowercase().as_str())
}

/// The hub's `type/subtype` rule (letters, digits, `.`, `+`, `-`).
fn valid_mime(mime: &str) -> bool {
    let Some((kind, subtype)) = mime.split_once('/') else {
        return false;
    };
    let token = |part: &str| {
        !part.is_empty()
            && part.len() <= 127
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
    };
    token(kind) && token(subtype)
}

/// The checks every file passes before it becomes a pending attachment: a size of 1 byte to
/// 20 MB and a type that is not a program. Nothing has been read or uploaded yet.
pub fn check(name: &str, mime: &str, size: u64) -> Result<(), Reject> {
    if size == 0 {
        return Err(Reject::Empty { name: name.into() });
    }
    if size > MAX_BYTES {
        return Err(Reject::TooLarge {
            name: name.into(),
            size,
        });
    }
    if refused_type(name, mime) {
        return Err(Reject::Type {
            name: name.into(),
            mime: mime.into(),
        });
    }
    Ok(())
}

/// A file read from disk, ready to be added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Read the file at `path`. The size and type are checked from the metadata first, so a huge
/// file or a program is never read.
pub fn read_file(path: &Path) -> Result<Picked, Reject> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let unreadable = |error: std::io::Error| Reject::Unreadable {
        name: name.clone(),
        detail: error.to_string(),
    };
    let metadata = std::fs::metadata(path).map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(Reject::Unreadable {
            name: name.clone(),
            detail: "not a file".to_owned(),
        });
    }
    let mime = guess_mime(&name);
    check(&name, &mime, metadata.len())?;
    let bytes = std::fs::read(path).map_err(unreadable)?;
    Ok(Picked { name, mime, bytes })
}

// ---- upload ---------------------------------------------------------------------------------

/// Upload progress, 0 to 100, shared between the uploader's thread and the view.
#[derive(Debug, Clone, Default)]
pub struct Progress(Arc<AtomicU8>);

impl Progress {
    pub fn set(&self, percent: u8) {
        self.0.store(percent.min(100), Ordering::Relaxed);
    }

    pub fn get(&self) -> u8 {
        self.0.load(Ordering::Relaxed)
    }
}

/// What an [`Uploader`] is given.
#[derive(Debug, Clone)]
pub struct UploadJob {
    pub name: String,
    pub mime: String,
    pub bytes: Arc<Vec<u8>>,
    /// Stable per attachment, so a retry after a lost answer replays the hub's answer.
    pub idempotency_key: String,
    pub progress: Progress,
}

/// Why an upload failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadError {
    /// Signed out: there is no hub to upload to.
    NoHub,
    Transport(String),
    /// The hub (or the storage URL it signed) answered with an error.
    Refused {
        status: u16,
        code: String,
        message: String,
    },
    TooLarge,
    Malformed,
}

impl UploadError {
    /// A short reason for the chip, from the Fluent catalog.
    pub fn detail(&self) -> String {
        match self {
            Self::NoHub => tr("compose-attach-error-no-hub"),
            Self::Transport(detail) => {
                tr_args("compose-attach-error-transport", &[("detail", detail)])
            }
            Self::Refused {
                status,
                code,
                message,
            } => {
                let detail = if message.is_empty() { code } else { message };
                tr_args(
                    "compose-attach-error-refused",
                    &[("status", &status.to_string()), ("detail", detail)],
                )
            }
            Self::TooLarge => tr("compose-attach-error-too-large"),
            Self::Malformed => tr("compose-attach-error-malformed"),
        }
    }
}

pub type UploadFuture = Pin<Box<dyn Future<Output = Result<ContentPart, UploadError>> + Send>>;

/// Uploads one attachment and answers the `{type:"file"}` part that references it. Runs on its
/// own thread and runtime (the views never touch tokio).
pub trait Uploader: Send + Sync + 'static {
    fn upload(&self, job: UploadJob) -> UploadFuture;
}

/// The uploader of a signed-out composer: every upload fails with [`UploadError::NoHub`].
pub struct NoUploader;

impl Uploader for NoUploader {
    fn upload(&self, _: UploadJob) -> UploadFuture {
        Box::pin(async { Err(UploadError::NoHub) })
    }
}

/// `POST {api}/attachments`, then `PUT` the bytes to the signed URL it answers.
pub struct HttpUploader {
    api: String,
    token: Option<String>,
}

impl HttpUploader {
    /// `api` is the `…/v1` base URL.
    pub fn new(api: impl Into<String>, token: Option<String>) -> Self {
        Self {
            api: api.into(),
            token,
        }
    }
}

impl Uploader for HttpUploader {
    fn upload(&self, job: UploadJob) -> UploadFuture {
        let api = self.api.clone();
        let token = self.token.clone();
        Box::pin(async move { upload_http(&api, token.as_deref(), job).await })
    }
}

fn refused(status: u16, body: &Value) -> UploadError {
    if status == 413 {
        return UploadError::TooLarge;
    }
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    UploadError::Refused {
        status,
        code: text("error"),
        message: text("message"),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

async fn upload_http(
    api: &str,
    token: Option<&str>,
    job: UploadJob,
) -> Result<ContentPart, UploadError> {
    let transport = |error: reqwest::Error| UploadError::Transport(error.without_url().to_string());
    let client = reqwest::Client::new();
    let size = job.bytes.len() as u64;
    let mut create = client
        .post(format!("{}/attachments", api.trim_end_matches('/')))
        .header("Idempotency-Key", &job.idempotency_key)
        .json(&json!({ "name": job.name, "mime_type": job.mime, "size": size }));
    if let Some(token) = token {
        create = create.bearer_auth(token);
    }
    let response = create.send().await.map_err(transport)?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(transport)?;
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status != 201 {
        return Err(refused(status, &body));
    }
    let field = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
    let (Some(upload_url), Some(url)) = (field("upload_url"), field("url")) else {
        return Err(UploadError::Malformed);
    };
    job.progress.set(20);
    let put = client
        .put(upload_url)
        .header("Content-Type", &job.mime)
        .body(job.bytes.as_ref().clone())
        .send()
        .await
        .map_err(transport)?;
    let status = put.status().as_u16();
    if !(200..300).contains(&status) {
        let text = put.text().await.unwrap_or_default();
        return Err(refused(
            status,
            &serde_json::from_str(&text).unwrap_or(Value::Null),
        ));
    }
    job.progress.set(100);
    Ok(ContentPart::File {
        name: job.name,
        mime_type: job.mime,
        url,
        size: Some(size),
        sha256: Some(hex(&Sha256::digest(job.bytes.as_slice()))),
    })
}

/// `key` made specific to the attachments a request carries: the same files give the same key
/// (a retry replays), other files another, so the hub never sees one key with two bodies.
pub fn keyed_by_parts(key: &str, parts: &[ContentPart]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        if let ContentPart::File { url, .. } = part {
            hasher.update(url.as_bytes());
            hasher.update(b"\n");
        }
    }
    let digest = hex(&hasher.finalize());
    let base: String = key.chars().take(100).collect();
    format!("{base}.a{}", &digest[..16])
}

// ---- the model ------------------------------------------------------------------------------

/// Where one attachment is.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// Added; uploads when the message is sent.
    Pending,
    Uploading,
    Uploaded(ContentPart),
    Failed(String),
}

/// One file the user attached.
#[derive(Debug, Clone)]
pub struct Item {
    pub id: u64,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub state: State,
    pub progress: Progress,
    bytes: Arc<Vec<u8>>,
    key: String,
}

impl Item {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

/// Whether a send may go.
#[derive(Debug, Clone, PartialEq)]
pub enum Gate {
    /// Nothing attached: send the text alone.
    Empty,
    /// Every attachment is uploaded; these are the parts, in the order attached.
    Ready(Vec<ContentPart>),
    /// Uploads are running (or about to); the send goes when they finish.
    Waiting,
    /// At least one upload failed; retry or remove it first.
    Blocked,
}

#[derive(Debug, Default)]
pub struct AttachmentSet {
    items: Vec<Item>,
    next_id: u64,
}

impl AttachmentSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn get(&self, id: u64) -> Option<&Item> {
        self.items.iter().find(|item| item.id == id)
    }

    /// Add a file already in memory. The name is trimmed; a missing or invalid MIME type
    /// becomes `application/octet-stream`.
    pub fn add(&mut self, name: &str, mime: &str, bytes: Vec<u8>) -> Result<u64, Reject> {
        let name: String = name
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_NAME_CHARS)
            .collect();
        let name = if name.is_empty() {
            "attachment".to_owned()
        } else {
            name
        };
        let mime = mime.trim().to_ascii_lowercase();
        let mime = if valid_mime(&mime) {
            mime
        } else {
            "application/octet-stream".to_owned()
        };
        check(&name, &mime, bytes.len() as u64)?;
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Item {
            id,
            size: bytes.len() as u64,
            name,
            mime,
            state: State::Pending,
            progress: Progress::default(),
            bytes: Arc::new(bytes),
            key: client_core::outbox::new_idempotency_key(),
        });
        Ok(id)
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|item| item.id != id);
        self.items.len() != before
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Ids of attachments that have not started uploading.
    pub fn pending_ids(&self) -> Vec<u64> {
        self.items
            .iter()
            .filter(|item| item.state == State::Pending)
            .map(|item| item.id)
            .collect()
    }

    /// `true` while any upload runs.
    pub fn is_uploading(&self) -> bool {
        self.items.iter().any(|item| item.state == State::Uploading)
    }

    /// Start uploading `id` (it must be pending): the job to run.
    pub fn begin(&mut self, id: u64) -> Option<UploadJob> {
        let item = self.items.iter_mut().find(|item| item.id == id)?;
        if item.state != State::Pending {
            return None;
        }
        item.state = State::Uploading;
        item.progress.set(0);
        Some(UploadJob {
            name: item.name.clone(),
            mime: item.mime.clone(),
            bytes: item.bytes.clone(),
            idempotency_key: item.key.clone(),
            progress: item.progress.clone(),
        })
    }

    /// The upload of `id` ended. An attachment removed meanwhile is ignored.
    pub fn finish(&mut self, id: u64, result: Result<ContentPart, UploadError>) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.state = match result {
                Ok(part) => State::Uploaded(part),
                Err(error) => State::Failed(error.detail()),
            };
        }
    }

    /// Put a failed attachment back to pending (so it uploads again). `false` when `id` is not
    /// failed.
    pub fn retry(&mut self, id: u64) -> bool {
        match self.items.iter_mut().find(|item| item.id == id) {
            Some(item) if matches!(item.state, State::Failed(_)) => {
                item.state = State::Pending;
                true
            }
            _ => false,
        }
    }

    pub fn gate(&self) -> Gate {
        if self.items.is_empty() {
            return Gate::Empty;
        }
        if self
            .items
            .iter()
            .any(|item| matches!(item.state, State::Failed(_)))
        {
            return Gate::Blocked;
        }
        let parts: Vec<ContentPart> = self
            .items
            .iter()
            .filter_map(|item| match &item.state {
                State::Uploaded(part) => Some(part.clone()),
                _ => None,
            })
            .collect();
        if parts.len() == self.items.len() {
            Gate::Ready(parts)
        } else {
            Gate::Waiting
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_binary_units() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(MAX_BYTES), "20.0 MB");
    }

    #[test]
    fn programs_are_refused_by_extension_and_by_type() {
        assert!(refused_type("setup.EXE", "application/octet-stream"));
        assert!(refused_type("tool", "application/x-msdownload"));
        assert!(!refused_type("notes.md", "text/markdown"));
    }
}
