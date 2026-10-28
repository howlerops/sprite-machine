//! User file parts → ACP prompt blocks (PLAN-v2 §4.1, client-api.md §5.6).
//!
//! A `{type:"file"}` part is a signed https URL the hub made reachable. [`prompt_blocks`]
//! fetches each one and turns it into ACP content:
//!
//! - an image (`image/png`, `image/jpeg`, `image/gif`, `image/webp`) becomes an ACP `image`
//!   block with the bytes base64-encoded;
//! - any other file is written to `<workspace>/.sm/attachments/<sanitized name>` and the
//!   prompt carries an `@.sm/attachments/<name>` reference, relative to the workspace.
//!
//! Fetching is guarded:
//!
//! - only `https` URLs are fetched ([`AttachmentError::NotHttps`]);
//! - the host is resolved here and every resolved address must be public
//!   ([`AttachmentError::NonPublicAddress`]); the connection is pinned to the addresses that
//!   were checked, and redirects are not followed, so a later DNS answer cannot move the
//!   request to a private address;
//! - the body is capped (default 20 MB, `Content-Length` checked up front and the streamed
//!   byte count checked as it arrives), and an optional `sha256` is verified.
//!
//! A failure is reported as an [`AttachmentError`] whose [`AttachmentError::code`] is
//! `attachment`; the caller fails the task with it before any prompt reaches the agent. Error
//! messages never contain the URL, since a signed URL carries a credential in its query.
//!
//! **Test-only allowance.** [`FilePolicy::insecure_loopback_for_tests`] lets the `http` scheme
//! and loopback addresses through, so the tests can use a local plain-HTTP server. The
//! production default ([`FilePolicy::default`]) never sets it, and no production code path
//! constructs a policy with it.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use protocol::acp::ContentBlock;
use protocol::tunnel::ContentPart;
use reqwest::Url;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::acp::fs::{FsError, Workspace};

/// Default cap on one attachment (client-api.md §5.6: ≤ 20 MB).
pub const DEFAULT_MAX_BYTES: u64 = 20 * 1024 * 1024;

/// The workspace directory non-image attachments are written to.
pub const ATTACHMENT_DIR: &str = ".sm/attachments";

/// The `RunError` code of a failed attachment.
pub const ATTACHMENT_CODE: &str = "attachment";

/// Maximum longest name kept, in characters, before the extension.
const MAX_STEM_CHARS: usize = 100;

/// Most suffixes tried when a name is taken.
const MAX_DEDUPE: u32 = 1_000;

/// Host resolution. The default asks the system resolver; tests inject their own.
pub type Resolver = Arc<dyn Fn(&str, u16) -> io::Result<Vec<IpAddr>> + Send + Sync>;

/// What the attachment fetch is allowed to do.
#[derive(Clone)]
pub struct FilePolicy {
    /// Largest attachment accepted, in bytes.
    pub max_bytes: u64,
    /// Whole-download timeout.
    pub timeout: Duration,
    /// TEST ONLY. Allows `http` URLs and loopback addresses. Production never sets this:
    /// [`FilePolicy::default`] leaves it `false`.
    pub insecure_loopback_for_tests: bool,
    /// Host resolver. `None` uses the system resolver.
    pub resolver: Option<Resolver>,
}

impl Default for FilePolicy {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            timeout: Duration::from_secs(60),
            insecure_loopback_for_tests: false,
            resolver: None,
        }
    }
}

/// Why a file part could not become prompt content.
#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("attachment {name}: URL must be https")]
    NotHttps { name: String },
    #[error("attachment {name}: URL is not valid")]
    InvalidUrl { name: String },
    #[error("attachment {name}: host did not resolve")]
    Unresolved { name: String },
    #[error("attachment {name}: host resolves to a non-public address")]
    NonPublicAddress { name: String },
    #[error("attachment {name}: larger than {max} bytes")]
    TooLarge { name: String, max: u64 },
    #[error("attachment {name}: download failed: {message}")]
    Download { name: String, message: String },
    #[error("attachment {name}: server answered HTTP {status}")]
    Status { name: String, status: u16 },
    #[error("attachment {name}: sha256 does not match")]
    Sha256Mismatch { name: String },
    #[error("attachment {name}: could not be stored: {message}")]
    Store { name: String, message: String },
    #[error("prompt content could not be encoded: {0}")]
    Encode(#[from] serde_json::Error),
}

impl AttachmentError {
    /// The `RunError` code: `attachment`, or `protocol` for a content that cannot be encoded.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Encode(_) => "protocol",
            _ => ATTACHMENT_CODE,
        }
    }
}

/// Turns the parts of a `prompt`/`dispatch` into ACP content blocks, in order. Text parts
/// pass through; file parts are fetched and stored as described in the module docs.
pub async fn prompt_blocks(
    parts: &[ContentPart],
    workspace: &Workspace,
    policy: &FilePolicy,
) -> Result<Vec<ContentBlock>, AttachmentError> {
    let mut blocks = Vec::with_capacity(parts.len());
    for part in parts {
        let value = match part {
            ContentPart::Text { text } => json!({ "type": "text", "text": text }),
            ContentPart::File {
                name,
                mime_type,
                url,
                size,
                sha256,
            } => {
                file_block(
                    name,
                    mime_type,
                    url,
                    *size,
                    sha256.as_deref(),
                    workspace,
                    policy,
                )
                .await?
            }
        };
        blocks.push(serde_json::from_value(value)?);
    }
    Ok(blocks)
}

async fn file_block(
    name: &str,
    mime_type: &str,
    url: &str,
    size: Option<u64>,
    sha256: Option<&str>,
    workspace: &Workspace,
    policy: &FilePolicy,
) -> Result<serde_json::Value, AttachmentError> {
    if let Some(size) = size
        && size > policy.max_bytes
    {
        return Err(AttachmentError::TooLarge {
            name: name.to_owned(),
            max: policy.max_bytes,
        });
    }
    let bytes = fetch(name, url, policy).await?;
    if let Some(expected) = sha256
        && !sha256_hex(&bytes).eq_ignore_ascii_case(expected.trim())
    {
        return Err(AttachmentError::Sha256Mismatch {
            name: name.to_owned(),
        });
    }
    if let Some(image) = image_mime(mime_type) {
        return Ok(json!({
            "type": "image",
            "data": STANDARD.encode(&bytes),
            "mimeType": image,
        }));
    }
    let relative = store(name, &bytes, workspace).await?;
    Ok(json!({ "type": "text", "text": format!("@{relative}") }))
}

/// The canonical image MIME type, if `mime_type` names one of the four image types ACP
/// image blocks carry. Parameters (`; charset=…`) and case are ignored.
pub fn image_mime(mime_type: &str) -> Option<&'static str> {
    let essence = mime_type.split(';').next()?.trim().to_ascii_lowercase();
    match essence.as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// Downloads an attachment under the policy. Returns the body, at most `policy.max_bytes`.
pub async fn fetch(name: &str, url: &str, policy: &FilePolicy) -> Result<Vec<u8>, AttachmentError> {
    let invalid = || AttachmentError::InvalidUrl {
        name: name.to_owned(),
    };
    let parsed = Url::parse(url).map_err(|_| invalid())?;
    match parsed.scheme() {
        "https" => {}
        "http" if policy.insecure_loopback_for_tests => {}
        _ => {
            return Err(AttachmentError::NotHttps {
                name: name.to_owned(),
            });
        }
    }
    let host = parsed.host_str().ok_or_else(invalid)?;
    let port = parsed.port_or_known_default().ok_or_else(invalid)?;

    // An IP literal needs no lookup; a name is resolved here so its addresses can be checked
    // and then pinned on the connection.
    let literal = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .ok();
    let addresses = match literal {
        Some(ip) => vec![ip],
        None => resolve(name, host, port, policy).await?,
    };
    if addresses.is_empty() {
        return Err(AttachmentError::Unresolved {
            name: name.to_owned(),
        });
    }
    if addresses
        .iter()
        .any(|ip| !allowed_address(*ip, policy.insecure_loopback_for_tests))
    {
        return Err(AttachmentError::NonPublicAddress {
            name: name.to_owned(),
        });
    }

    let mut builder = reqwest::Client::builder()
        // A proxy would connect to a host this check did not see; connect directly.
        .no_proxy()
        // A redirect could lead to an address that was never checked.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(policy.timeout);
    if literal.is_none() {
        let pinned: Vec<SocketAddr> = addresses
            .iter()
            .map(|ip| SocketAddr::new(*ip, port))
            .collect();
        builder = builder.resolve_to_addrs(host, &pinned);
    }
    let client = builder.build().map_err(|error| AttachmentError::Download {
        name: name.to_owned(),
        message: error.without_url().to_string(),
    })?;

    let download = |error: reqwest::Error| AttachmentError::Download {
        name: name.to_owned(),
        message: error.without_url().to_string(),
    };
    let mut response = client.get(parsed.clone()).send().await.map_err(download)?;
    let status = response.status();
    if !status.is_success() {
        return Err(AttachmentError::Status {
            name: name.to_owned(),
            status: status.as_u16(),
        });
    }
    let too_large = || AttachmentError::TooLarge {
        name: name.to_owned(),
        max: policy.max_bytes,
    };
    if response
        .content_length()
        .is_some_and(|length| length > policy.max_bytes)
    {
        return Err(too_large());
    }
    // Without a Content-Length the body is counted as it streams in; the download stops at
    // the first chunk that crosses the cap.
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(download)? {
        let total = body.len() as u64 + chunk.len() as u64;
        if total > policy.max_bytes {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn resolve(
    name: &str,
    host: &str,
    port: u16,
    policy: &FilePolicy,
) -> Result<Vec<IpAddr>, AttachmentError> {
    let resolved = match &policy.resolver {
        Some(resolver) => resolver(host, port),
        None => tokio::net::lookup_host((host, port))
            .await
            .map(|addresses| addresses.map(|address| address.ip()).collect()),
    };
    resolved.map_err(|_| AttachmentError::Unresolved {
        name: name.to_owned(),
    })
}

/// Whether a connection to `ip` is allowed: public addresses always, loopback only under the
/// test allowance.
fn allowed_address(ip: IpAddr, insecure_loopback_for_tests: bool) -> bool {
    if insecure_loopback_for_tests && ip.is_loopback() {
        return true;
    }
    is_public(ip)
}

/// Whether `ip` is a globally routable address. Private, loopback, link-local, shared
/// (CGNAT), documentation, benchmarking, multicast, reserved and unspecified addresses are
/// not. IPv4-mapped and NAT64 IPv6 addresses are judged by their embedded IPv4 address.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let shared = a == 100 && (64..=127).contains(&b);
    let benchmarking = a == 198 && (b == 18 || b == 19);
    let documentation = (a == 192 && b == 0 && c == 2)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113);
    !(a == 0
        || a == 10
        || a == 127
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && c == 0)
        || shared
        || benchmarking
        || documentation
        || a >= 224)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    // NAT64 well-known prefix 64:ff9b::/96 carries an IPv4 address in its last 32 bits.
    if segments[0] == 0x64 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0] {
        let [a, b] = [segments[6], segments[7]];
        let v4 = Ipv4Addr::new((a >> 8) as u8, a as u8, (b >> 8) as u8, b as u8);
        return is_public_v4(v4);
    }
    let unique_local = (segments[0] & 0xfe00) == 0xfc00;
    let link_local = (segments[0] & 0xffc0) == 0xfe80;
    let multicast = (segments[0] & 0xff00) == 0xff00;
    let documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
    // `::/96` (the deprecated IPv4-compatible form) and `::` itself.
    let compatible = segments[..6] == [0, 0, 0, 0, 0, 0];
    !(ip.is_unspecified()
        || ip.is_loopback()
        || compatible
        || unique_local
        || link_local
        || multicast
        || documentation)
}

/// Writes an attachment into `<workspace>/.sm/attachments/` under a sanitized name, never
/// over an existing file. Returns the path relative to the workspace.
async fn store(name: &str, bytes: &[u8], workspace: &Workspace) -> Result<String, AttachmentError> {
    let store_error = |message: String| AttachmentError::Store {
        name: name.to_owned(),
        message,
    };
    let dir = workspace
        .resolve(Path::new(ATTACHMENT_DIR))
        .map_err(|error: FsError| store_error(error.to_string()))?;
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| store_error(error.to_string()))?;

    let base = sanitize_name(name);
    let (stem, extension) = split_extension(&base);
    for attempt in 1..=MAX_DEDUPE {
        let file_name = match (attempt, extension) {
            (1, _) => base.clone(),
            (n, Some(ext)) => format!("{stem}-{n}.{ext}"),
            (n, None) => format!("{stem}-{n}"),
        };
        let path: PathBuf = dir.join(&file_name);
        let opened = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await;
        match opened {
            Ok(mut file) => {
                file.write_all(bytes)
                    .await
                    .map_err(|error| store_error(error.to_string()))?;
                file.flush()
                    .await
                    .map_err(|error| store_error(error.to_string()))?;
                return Ok(format!("{ATTACHMENT_DIR}/{file_name}"));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(store_error(error.to_string())),
        }
    }
    Err(store_error("too many files with this name".to_owned()))
}

/// A file name that is safe to create inside the attachments directory: the last path
/// component of `name`, reduced to ASCII letters, digits, `.`, `-` and `_`, with no leading
/// dot (so no `..`, no hidden file) and never empty.
pub fn sanitize_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mapped: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = mapped.trim_start_matches('.');
    if trimmed.is_empty() {
        return "attachment".to_owned();
    }
    let (stem, extension) = split_extension(trimmed);
    let stem: String = stem.chars().take(MAX_STEM_CHARS).collect();
    match extension {
        Some(ext) if !stem.is_empty() => format!("{stem}.{ext}"),
        _ => stem,
    }
}

/// Splits `name` at its last dot. A dot at the start or the end gives no extension.
fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(index) if index > 0 && index + 1 < name.len() => {
            (&name[..index], Some(&name[index + 1..]))
        }
        _ => (name, None),
    }
}
