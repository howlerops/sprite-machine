//! Tests for user file parts → ACP prompt blocks (`bridge::files`, PLAN-v2 §4.1).
//!
//! Downloads go to a local plain-HTTP server (a raw TCP listener, so the chunked and redirect
//! cases are exact). Those tests set [`FilePolicy::insecure_loopback_for_tests`], the explicit
//! test-only allowance; the production default never sets it, and a test checks that default.
//! The SSRF tests run under the production default, or inject a resolver.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use protocol::tunnel::ContentPart;
use sidecar::acp::fs::Workspace;
use sidecar::bridge::files::{
    AttachmentError, DEFAULT_MAX_BYTES, FilePolicy, is_public, prompt_blocks, sanitize_name,
    sha256_hex,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-image-bytes";
const NOTES: &str = "hello attachments";

// ---- a local HTTP server -----------------------------------------------------------------------

struct Reply {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    /// Sent with `Transfer-Encoding: chunked` and no `Content-Length`.
    chunked: bool,
    location: Option<&'static str>,
}

fn ok(content_type: &'static str, body: impl Into<Vec<u8>>) -> Reply {
    Reply {
        status: 200,
        reason: "OK",
        content_type,
        body: body.into(),
        chunked: false,
        location: None,
    }
}

fn reply_for(path: &str) -> Reply {
    match path {
        "/image.png" => ok("image/png", PNG),
        "/notes.txt" => ok("text/plain", NOTES),
        "/big" => ok("application/octet-stream", vec![7u8; 100]),
        "/big-chunked" => Reply {
            chunked: true,
            ..ok("application/octet-stream", vec![7u8; 100])
        },
        "/redirect" => Reply {
            status: 302,
            reason: "Found",
            location: Some("/notes.txt"),
            ..ok("text/plain", Vec::new())
        },
        _ => Reply {
            status: 404,
            reason: "Not Found",
            ..ok("text/plain", "nope")
        },
    }
}

fn wire(reply: &Reply) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nConnection: close\r\n",
        reply.status, reply.reason, reply.content_type
    )
    .into_bytes();
    if let Some(location) = reply.location {
        out.extend(format!("Location: {location}\r\n").bytes());
    }
    if reply.chunked {
        out.extend(b"Transfer-Encoding: chunked\r\n\r\n");
        for chunk in reply.body.chunks(10) {
            out.extend(format!("{:x}\r\n", chunk.len()).bytes());
            out.extend(chunk);
            out.extend(b"\r\n");
        }
        out.extend(b"0\r\n\r\n");
    } else {
        out.extend(format!("Content-Length: {}\r\n\r\n", reply.body.len()).bytes());
        out.extend(&reply.body);
    }
    out
}

/// Starts the test server on 127.0.0.1 with an ephemeral port and returns its address.
async fn serve() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&request).into_owned();
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let _ = socket.write_all(&wire(&reply_for(&path))).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    addr
}

// ---- helpers -----------------------------------------------------------------------------------

fn file(name: &str, mime_type: &str, url: String) -> ContentPart {
    ContentPart::File {
        name: name.to_owned(),
        mime_type: mime_type.to_owned(),
        url,
        size: None,
        sha256: None,
    }
}

/// The policy the local test server needs: plain http and loopback allowed.
fn test_policy() -> FilePolicy {
    FilePolicy {
        insecure_loopback_for_tests: true,
        ..FilePolicy::default()
    }
}

fn workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = Workspace::new(dir.path()).expect("workspace");
    (dir, workspace)
}

fn attachments_dir(root: &Path) -> std::path::PathBuf {
    root.join(".sm").join("attachments")
}

fn ip(text: &str) -> IpAddr {
    text.parse().expect("ip literal")
}

fn is_attachment_error(error: &AttachmentError) -> bool {
    error.code() == "attachment"
}

// ---- the criteria ------------------------------------------------------------------------------

#[tokio::test]
async fn image_file_becomes_an_image_block() {
    let addr = serve().await;
    let (dir, ws) = workspace();
    let parts = [file(
        "pic.png",
        "image/png",
        format!("http://{addr}/image.png"),
    )];
    let blocks = prompt_blocks(&parts, &ws, &test_policy())
        .await
        .expect("image");
    assert_eq!(blocks.len(), 1);
    let value = serde_json::to_value(&blocks[0]).expect("json");
    assert_eq!(value["type"], "image");
    assert_eq!(value["mimeType"], "image/png");
    assert_eq!(value["data"], STANDARD.encode(PNG));
    // Images are not written to the workspace.
    assert!(!attachments_dir(dir.path()).exists());
}

#[tokio::test]
async fn text_file_is_stored_under_sm_attachments_and_referenced() {
    let addr = serve().await;
    let (dir, ws) = workspace();
    let parts = [
        ContentPart::Text {
            text: "read this".to_owned(),
        },
        file(
            "notes.txt",
            "text/plain",
            format!("http://{addr}/notes.txt"),
        ),
    ];
    let blocks = prompt_blocks(&parts, &ws, &test_policy())
        .await
        .expect("text file");
    assert_eq!(blocks.len(), 2);
    let text = serde_json::to_value(&blocks[0]).expect("json");
    assert_eq!(text["type"], "text");
    assert_eq!(text["text"], "read this");
    let reference = serde_json::to_value(&blocks[1]).expect("json");
    assert_eq!(reference["type"], "text");
    assert_eq!(reference["text"], "@.sm/attachments/notes.txt");
    let stored = std::fs::read_to_string(attachments_dir(dir.path()).join("notes.txt"))
        .expect("stored file");
    assert_eq!(stored, NOTES);
}

#[tokio::test]
async fn size_cap_is_enforced_by_content_length() {
    let addr = serve().await;
    let (dir, ws) = workspace();
    let policy = FilePolicy {
        max_bytes: 50,
        ..test_policy()
    };
    let parts = [file(
        "big.bin",
        "application/octet-stream",
        format!("http://{addr}/big"),
    )];
    let error = prompt_blocks(&parts, &ws, &policy)
        .await
        .expect_err("over the cap");
    assert!(matches!(error, AttachmentError::TooLarge { max: 50, .. }));
    assert!(is_attachment_error(&error));
    assert!(!attachments_dir(dir.path()).join("big.bin").exists());
}

#[tokio::test]
async fn size_cap_is_enforced_while_streaming_without_content_length() {
    let addr = serve().await;
    let (_dir, ws) = workspace();
    let url = format!("http://{addr}/big-chunked");

    let over = FilePolicy {
        max_bytes: 50,
        ..test_policy()
    };
    let error = prompt_blocks(
        &[file("big.bin", "application/octet-stream", url.clone())],
        &ws,
        &over,
    )
    .await
    .expect_err("streamed past the cap");
    assert!(matches!(error, AttachmentError::TooLarge { .. }));

    // Exactly the cap is accepted.
    let exact = FilePolicy {
        max_bytes: 100,
        ..test_policy()
    };
    prompt_blocks(
        &[file("big.bin", "application/octet-stream", url)],
        &ws,
        &exact,
    )
    .await
    .expect("body at the cap");
}

#[tokio::test]
async fn declared_size_over_the_cap_is_refused_before_any_download() {
    let (_dir, ws) = workspace();
    // Nothing listens on this port: the size check must answer before any connection.
    let part = ContentPart::File {
        name: "huge.bin".to_owned(),
        mime_type: "application/octet-stream".to_owned(),
        url: "https://127.0.0.1:1/huge".to_owned(),
        size: Some(DEFAULT_MAX_BYTES + 1),
        sha256: None,
    };
    let error = prompt_blocks(&[part], &ws, &FilePolicy::default())
        .await
        .expect_err("declared over the cap");
    assert!(matches!(error, AttachmentError::TooLarge { .. }));
}

#[tokio::test]
async fn sha256_is_checked_when_given() {
    let addr = serve().await;
    let (dir, ws) = workspace();
    let url = format!("http://{addr}/notes.txt");
    let digest = sha256_hex(NOTES.as_bytes());

    let good = ContentPart::File {
        name: "notes.txt".to_owned(),
        mime_type: "text/plain".to_owned(),
        url: url.clone(),
        size: None,
        sha256: Some(digest.to_uppercase()),
    };
    prompt_blocks(&[good], &ws, &test_policy())
        .await
        .expect("digest matches (case-insensitive)");

    let bad = ContentPart::File {
        name: "other.txt".to_owned(),
        mime_type: "text/plain".to_owned(),
        url,
        size: None,
        sha256: Some("00".repeat(32)),
    };
    let error = prompt_blocks(&[bad], &ws, &test_policy())
        .await
        .expect_err("digest mismatch");
    assert!(matches!(error, AttachmentError::Sha256Mismatch { .. }));
    assert!(is_attachment_error(&error));
    assert!(!attachments_dir(dir.path()).join("other.txt").exists());
}

#[tokio::test]
async fn non_https_urls_are_rejected_under_the_production_default() {
    let (_dir, ws) = workspace();
    for url in [
        "http://example.com/a.txt",
        "file:///etc/passwd",
        "data:text/plain,hello",
        "ftp://example.com/a.txt",
    ] {
        let parts = [file("a.txt", "text/plain", url.to_owned())];
        let error = prompt_blocks(&parts, &ws, &FilePolicy::default())
            .await
            .expect_err(url);
        assert!(
            matches!(error, AttachmentError::NotHttps { .. }),
            "{url}: {error}"
        );
        assert_eq!(error.code(), "attachment");
    }
}

#[tokio::test]
async fn private_and_loopback_addresses_are_rejected() {
    let (_dir, ws) = workspace();
    let production = FilePolicy::default();

    // IP literals: loopback, unique-local, metadata link-local, unspecified.
    for url in [
        "https://127.0.0.1:1/x",
        "https://[::1]/x",
        "https://169.254.169.254/latest",
        "https://10.0.0.5/x",
        "https://0.0.0.0/x",
    ] {
        let error = prompt_blocks(&[file("x", "text/plain", url.to_owned())], &ws, &production)
            .await
            .expect_err(url);
        assert!(
            matches!(error, AttachmentError::NonPublicAddress { .. }),
            "{url}: {error}"
        );
    }

    // The test allowance covers loopback only, not private ranges.
    let error = prompt_blocks(
        &[file("x", "text/plain", "http://10.0.0.5/x".to_owned())],
        &ws,
        &test_policy(),
    )
    .await
    .expect_err("private under the test flag");
    assert!(matches!(error, AttachmentError::NonPublicAddress { .. }));

    // A name whose resolved addresses include a private one is refused, pinned or not.
    let resolver: sidecar::bridge::files::Resolver =
        Arc::new(|host: &str, _port: u16| -> std::io::Result<Vec<IpAddr>> {
            Ok(match host {
                "files.example.test" => vec![ip("10.1.2.3")],
                "mixed.example.test" => vec![ip("93.184.216.34"), ip("192.168.0.1")],
                _ => Vec::new(),
            })
        });
    let policy = FilePolicy {
        resolver: Some(resolver),
        ..FilePolicy::default()
    };
    for host in ["files.example.test", "mixed.example.test"] {
        let url = format!("https://{host}/x");
        let error = prompt_blocks(&[file("x", "text/plain", url)], &ws, &policy)
            .await
            .expect_err(host);
        assert!(
            matches!(error, AttachmentError::NonPublicAddress { .. }),
            "{host}: {error}"
        );
    }
}

#[test]
fn address_classification_covers_the_non_public_ranges() {
    for ip in [
        "0.0.0.0",
        "10.1.2.3",
        "100.64.0.1",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1",
        "198.18.0.1",
        "192.0.2.1",
        "224.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "fc00::1",
        "fd12::1",
        "fe80::1",
        "ff02::1",
        "2001:db8::1",
        "::ffff:10.0.0.1",
        "64:ff9b::a00:1",
    ] {
        assert!(
            !is_public(ip.parse().expect("ip")),
            "{ip} must not be public"
        );
    }
    for ip in ["93.184.216.34", "8.8.8.8", "172.32.0.1", "2606:4700::1111"] {
        assert!(is_public(ip.parse().expect("ip")), "{ip} must be public");
    }
}

#[tokio::test]
async fn filename_traversal_is_sanitized() {
    let addr = serve().await;
    let (dir, ws) = workspace();
    // A probe name no other test uses, to show nothing was written beside the workspace.
    let probe = format!("traversal-probe-{}.txt", std::process::id());
    let escape = dir.path().parent().expect("parent").join(&probe);
    let url = format!("http://{addr}/notes.txt");

    let names = [
        (
            format!("../../{probe}"),
            "@.sm/attachments/".to_owned() + &probe,
        ),
        (
            "/etc/passwd".to_owned(),
            "@.sm/attachments/passwd".to_owned(),
        ),
        (
            "..\\..\\win.txt".to_owned(),
            "@.sm/attachments/win.txt".to_owned(),
        ),
        (".hidden".to_owned(), "@.sm/attachments/hidden".to_owned()),
        ("..".to_owned(), "@.sm/attachments/attachment".to_owned()),
        (
            "a b?c.txt".to_owned(),
            "@.sm/attachments/a_b_c.txt".to_owned(),
        ),
    ];
    for (name, expected) in names {
        let parts = [file(&name, "text/plain", url.clone())];
        let blocks = prompt_blocks(&parts, &ws, &test_policy())
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let value = serde_json::to_value(&blocks[0]).expect("json");
        assert_eq!(value["text"], expected, "name {name:?}");
    }

    assert!(
        !escape.exists(),
        "a traversal name wrote outside the workspace"
    );
    assert!(attachments_dir(dir.path()).join(&probe).exists());
    assert_eq!(sanitize_name("../../x/y.png"), "y.png");
    assert_eq!(sanitize_name("notes.tar.gz"), "notes.tar.gz");
    assert_eq!(sanitize_name(""), "attachment");
}

#[tokio::test]
async fn duplicate_names_get_a_suffix_and_keep_both_files() {
    let addr = serve().await;
    let (dir, ws) = workspace();
    let url = format!("http://{addr}/notes.txt");
    let parts = [
        file("notes.txt", "text/plain", url.clone()),
        file("notes.txt", "text/plain", url),
    ];
    let blocks = prompt_blocks(&parts, &ws, &test_policy())
        .await
        .expect("both files");
    let first = serde_json::to_value(&blocks[0]).expect("json");
    let second = serde_json::to_value(&blocks[1]).expect("json");
    assert_eq!(first["text"], "@.sm/attachments/notes.txt");
    assert_eq!(second["text"], "@.sm/attachments/notes-2.txt");
    let dir = attachments_dir(dir.path());
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).expect("first"),
        NOTES
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("notes-2.txt")).expect("second"),
        NOTES
    );
}

#[tokio::test]
async fn failed_downloads_fail_with_the_attachment_code() {
    let addr = serve().await;
    let (_dir, ws) = workspace();
    // A 404 and a redirect (redirects are not followed) both fail the attachment.
    for path in ["/missing", "/redirect"] {
        let url = format!("http://{addr}{path}");
        let error = prompt_blocks(&[file("x.txt", "text/plain", url)], &ws, &test_policy())
            .await
            .expect_err(path);
        assert!(
            matches!(error, AttachmentError::Status { .. }),
            "{path}: {error}"
        );
        assert_eq!(error.code(), "attachment");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn an_attachments_directory_that_links_outside_the_workspace_is_refused() {
    let addr = serve().await;
    let (dir, ws) = workspace();
    let outside = tempfile::tempdir().expect("outside");
    std::os::unix::fs::symlink(outside.path(), dir.path().join(".sm")).expect("symlink");
    let url = format!("http://{addr}/notes.txt");
    let error = prompt_blocks(&[file("notes.txt", "text/plain", url)], &ws, &test_policy())
        .await
        .expect_err("symlinked attachments dir");
    assert!(matches!(error, AttachmentError::Store { .. }));
    assert!(!outside.path().join("attachments").exists());
    assert!(
        std::fs::read_dir(outside.path())
            .expect("list")
            .next()
            .is_none()
    );
}

#[test]
fn production_default_never_allows_the_test_flag() {
    let policy = FilePolicy::default();
    assert!(!policy.insecure_loopback_for_tests);
    assert_eq!(policy.max_bytes, 20 * 1024 * 1024);
}
