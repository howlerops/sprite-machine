//! Inline images in the transcript (plan §3, surface 1: "image parts render inline").
//!
//! A `{type:"file"}` content part whose MIME type is a bitmap image and whose URL is http(s)
//! becomes a [`MediaRef`]; [`push_image_rows`] adds a [`RowKind::Image`] row per image after an
//! item's text rows. The pane shows a thumbnail on that row and the full image in a panel when
//! it is clicked.
//!
//! The bytes are fetched here, not by gpui's own image loader: the signed attachment URLs need
//! nothing but a GET, and fetching ourselves makes "the image could not be loaded" a state we
//! can show and test ([`LoadState::Failed`]) instead of a blank. [`ImageFetcher`] is injectable;
//! [`HttpFetcher`] is the real one. The cache keeps one state per URL, so a thumbnail and the
//! full view share one download.
//!
//! Only parts that reach the transcript as content parts are drawn this way (tool results
//! today). A user's own message keeps just its text in `client-core`'s `UserView`; see the
//! M3-006 report's discovered work.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use gpui_kit::TestSupportExt as _;
use gpui_kit::prelude::*;
use gpui_kit::*;
use protocol::tunnel::ContentPart;

use super::transcript::{Row, RowKind, Tone};
use crate::compose::{tr, tr_args};

/// Height of a row that holds a thumbnail.
pub const THUMB_ROW_HEIGHT: Pixels = px(112.);
/// The thumbnail fits in this box.
pub const THUMB_MAX: Pixels = px(96.);
/// The full-size view fits in this box at most.
pub const FULL_MAX: Pixels = px(900.);
/// Largest image fetched, in bytes (the hub's attachment limit).
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Element id of a laid-out image row's clickable thumbnail.
pub const THUMB_ID: &str = "transcript-image";
/// Element id of the thumbnail once its bytes are in.
pub const READY_ID: &str = "transcript-image-ready";
/// Element id of the note that replaces an image that cannot load.
pub const UNAVAILABLE_ID: &str = "transcript-image-unavailable";
/// Element id of the note shown while an image loads.
pub const LOADING_ID: &str = "transcript-image-loading";
/// Element id of the full-size panel.
pub const PANEL_ID: &str = "transcript-image-panel";
/// Element id of the full-size panel's close button.
pub const CLOSE_ID: &str = "transcript-image-close";

/// An image part: where to fetch it and what to call it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRef {
    pub name: String,
    pub mime: String,
    pub url: String,
}

/// `true` for the bitmap formats the pane draws.
pub fn is_inline_image(mime: &str) -> bool {
    matches!(
        ImageFormat::from_mime_type(&mime.to_ascii_lowercase()),
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Webp | ImageFormat::Gif)
    )
}

/// The image parts among `parts`, in order. A part without an http(s) URL is not one (the
/// pane never fetches a `file:` or `data:` URL a message names).
pub fn image_refs(parts: &[ContentPart]) -> Vec<MediaRef> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::File {
                name,
                mime_type,
                url,
                ..
            } if is_inline_image(mime_type)
                && (url.starts_with("https://") || url.starts_with("http://")) =>
            {
                Some(MediaRef {
                    name: super::transcript::sanitize(name),
                    mime: mime_type.to_ascii_lowercase(),
                    url: url.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

/// One [`RowKind::Image`] row per image part, indented by `indent`.
pub fn push_image_rows(parts: &[ContentPart], indent: u8, out: &mut Vec<Row>) {
    for media in image_refs(parts) {
        let mut row = Row::new(RowKind::Image, Tone::Normal, indent, media.name.clone());
        row.media = Some(media);
        out.push(row);
    }
}

/// The format `bytes` really are, from their first bytes; `None` for anything the pane does not
/// draw (a wrong `Content-Type`, an error page).
pub fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else {
        None
    }
}

pub type FetchFuture = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send>>;

/// Fetches an image's bytes. Runs on its own thread and runtime.
pub trait ImageFetcher: Send + Sync + 'static {
    fn fetch(&self, url: &str) -> FetchFuture;
}

/// A plain GET with a timeout, a status check and the size cap.
pub struct HttpFetcher;

impl ImageFetcher for HttpFetcher {
    fn fetch(&self, url: &str) -> FetchFuture {
        let url = url.to_owned();
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|error| error.to_string())?;
            let response = client
                .get(&url)
                .send()
                .await
                .map_err(|error| error.without_url().to_string())?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_IMAGE_BYTES as u64)
            {
                return Err("too large".to_owned());
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|error| error.without_url().to_string())?;
            if bytes.len() > MAX_IMAGE_BYTES {
                return Err("too large".to_owned());
            }
            Ok(bytes.to_vec())
        })
    }
}

/// Where one image is.
#[derive(Clone)]
pub enum LoadState {
    Loading,
    Ready(Arc<Image>),
    Failed(String),
}

impl LoadState {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

type Fetched = Result<Vec<u8>, String>;

/// One [`LoadState`] per URL.
pub struct MediaCache {
    fetcher: Arc<dyn ImageFetcher>,
    states: HashMap<String, LoadState>,
    jobs: Vec<(String, Receiver<Fetched>)>,
}

impl MediaCache {
    pub fn new(fetcher: Arc<dyn ImageFetcher>) -> Self {
        Self {
            fetcher,
            states: HashMap::new(),
            jobs: Vec::new(),
        }
    }

    pub fn set_fetcher(&mut self, fetcher: Arc<dyn ImageFetcher>) {
        self.fetcher = fetcher;
    }

    pub fn state(&self, url: &str) -> Option<&LoadState> {
        self.states.get(url)
    }

    /// Start fetching `url` unless it was already asked for. `true` when a fetch started.
    pub fn ensure(&mut self, url: &str) -> bool {
        if self.states.contains_key(url) {
            return false;
        }
        self.states.insert(url.to_owned(), LoadState::Loading);
        let future = self.fetcher.fetch(url);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let answer = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(future),
                Err(error) => Err(error.to_string()),
            };
            let _ = sender.send(answer);
        });
        self.jobs.push((url.to_owned(), receiver));
        true
    }

    /// Move finished fetches into the states. Returns `(changed, still_loading)`.
    pub fn pump(&mut self) -> (bool, bool) {
        let mut finished: Vec<(String, Fetched)> = Vec::new();
        self.jobs
            .retain(|(url, receiver)| match receiver.try_recv() {
                Ok(answer) => {
                    finished.push((url.clone(), answer));
                    false
                }
                Err(TryRecvError::Empty) => true,
                Err(TryRecvError::Disconnected) => {
                    finished.push((url.clone(), Err("the fetch did not finish".to_owned())));
                    false
                }
            });
        let changed = !finished.is_empty();
        for (url, answer) in finished {
            let state = match answer {
                Ok(bytes) => match sniff(&bytes) {
                    Some(format) => LoadState::Ready(Arc::new(Image::from_bytes(format, bytes))),
                    None => LoadState::Failed("not an image".to_owned()),
                },
                Err(detail) => LoadState::Failed(detail),
            };
            self.states.insert(url, state);
        }
        (changed, !self.jobs.is_empty())
    }
}

/// The note that stands in for an image that is loading or could not load.
fn note(id: &'static str, text: String) -> AnyElement {
    div()
        .id(id)
        .role(Role::Status)
        .test_support()
        .aria_label(SharedString::from(text.clone()))
        .text_sm()
        .child(text)
        .into_any_element()
}

/// The image inside a box of at most `max` on each side; a note when it is not (yet) there.
/// Also covers an image whose bytes the renderer cannot decode (the `img` fallback).
pub fn render_image(media: &MediaRef, state: Option<&LoadState>, max: Pixels) -> AnyElement {
    let name = media.name.clone();
    match state {
        Some(LoadState::Ready(image)) => {
            let unavailable = tr_args("compose-media-unavailable", &[("name", &name)]);
            div()
                .id(READY_ID)
                .test_support()
                .aria_label(SharedString::from(tr_args(
                    "compose-media-thumb-a11y",
                    &[("name", &name)],
                )))
                .child(
                    img(image.clone())
                        .max_w(max)
                        .max_h(max)
                        .with_fallback(move || note(UNAVAILABLE_ID, unavailable.clone())),
                )
                .into_any_element()
        }
        Some(LoadState::Failed(_)) => note(
            UNAVAILABLE_ID,
            tr_args("compose-media-unavailable", &[("name", &name)]),
        ),
        _ => note(
            LOADING_ID,
            tr_args("compose-media-loading", &[("name", &name)]),
        ),
    }
}

/// The label of the thumbnail's button.
pub fn open_label(media: &MediaRef) -> String {
    tr_args("compose-media-open-a11y", &[("name", &media.name)])
}

/// Text of the full-size panel's close button.
pub fn close_label() -> String {
    tr("compose-media-close")
}

#[cfg(test)]
mod tests {
    use super::{image_refs, sniff};
    use gpui_kit::ImageFormat;
    use protocol::tunnel::ContentPart;

    fn file(name: &str, mime: &str, url: &str) -> ContentPart {
        ContentPart::File {
            name: name.into(),
            mime_type: mime.into(),
            url: url.into(),
            size: None,
            sha256: None,
        }
    }

    #[test]
    fn only_bitmap_images_with_web_urls_are_inline() {
        let parts = [
            file("a.png", "image/png", "https://hub/x"),
            file("b.pdf", "application/pdf", "https://hub/y"),
            file("c.png", "image/png", "file:///etc/passwd"),
            file("d.svg", "image/svg+xml", "https://hub/z"),
            ContentPart::Text { text: "hi".into() },
        ];
        let refs = image_refs(&parts);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "a.png");
    }

    #[test]
    fn bytes_are_sniffed_not_trusted() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some(ImageFormat::Png));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(ImageFormat::Jpeg));
        assert_eq!(sniff(b"<html>nope</html>"), None);
    }
}
