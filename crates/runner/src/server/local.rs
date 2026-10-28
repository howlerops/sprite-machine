//! The local control socket of `sm rc`: `sm rc close <thread id>` asks the running server to end
//! an idle session, as an idle close does. It is a unix socket at `<state dir>/rc.sock`, mode
//! 0600, so only the owner of the state directory can talk to it. One request per connection,
//! one line each way: `close <thread id>` gets `ok`, or `err <reason>`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Longest request line read (a thread id is at most 128 bytes).
const MAX_LINE: u64 = 512;

/// The socket of the server whose state directory is `state_dir`.
pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("rc.sock")
}

/// A request from a local client, for the router to answer.
pub struct Request {
    pub thread_id: String,
    /// `Ok` once the session is closed, or the reason it was not.
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// The listening socket. Dropping it stops the accept loop and removes the socket.
pub struct Listener {
    path: PathBuf,
    task: JoinHandle<()>,
}

impl Listener {
    /// Binds the socket in `state_dir` and returns it with the requests it receives.
    pub fn bind(state_dir: &Path) -> Result<(Self, mpsc::UnboundedReceiver<Request>)> {
        let path = socket_path(state_dir);
        // A socket a crashed server left behind. A live one is another `sm rc` on this state.
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            bail!("another `sm rc` is running on {}", state_dir.display());
        }
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind the control socket {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict {}", path.display()))?;
        let (requests, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(accept(listener, requests));
        Ok((Self { path, task }, rx))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn accept(listener: UnixListener, requests: mpsc::UnboundedSender<Request>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let requests = requests.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, requests).await;
                });
            }
            // Out of descriptors, say: try again shortly rather than stop listening.
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
}

async fn serve(stream: UnixStream, requests: mpsc::UnboundedSender<Request>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read.take(MAX_LINE))
        .read_line(&mut line)
        .await?;
    let line = line.trim_end();
    let reply = match line.strip_prefix("close ") {
        Some(thread_id) => {
            let (reply, answer) = oneshot::channel();
            let request = Request {
                thread_id: thread_id.to_owned(),
                reply,
            };
            if requests.send(request).is_err() {
                Err("the server is stopping".to_owned())
            } else {
                answer
                    .await
                    .unwrap_or_else(|_| Err("the server stopped before it answered".to_owned()))
            }
        }
        None => Err(format!("unknown request {line:?}")),
    };
    let text = match reply {
        Ok(()) => "ok\n".to_owned(),
        Err(reason) => format!("err {reason}\n"),
    };
    write.write_all(text.as_bytes()).await?;
    Ok(())
}

/// `sm rc close <thread id>`: asks the `sm rc` running on `state_dir` to end the idle session of
/// `thread_id`.
pub async fn close_session(state_dir: &Path, thread_id: &str) -> Result<()> {
    if thread_id.is_empty() || thread_id.contains(char::is_whitespace) {
        bail!("a thread id has no spaces: {thread_id:?}");
    }
    let path = socket_path(state_dir);
    let stream = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "no `sm rc` is running for {} (no socket at {})",
            state_dir.display(),
            path.display()
        )
    })?;
    let (read, mut write) = stream.into_split();
    write
        .write_all(format!("close {thread_id}\n").as_bytes())
        .await?;
    let mut reply = String::new();
    BufReader::new(read.take(MAX_LINE))
        .read_line(&mut reply)
        .await?;
    match reply.trim_end() {
        "ok" => Ok(()),
        "" => bail!("the `sm rc` closed the connection without an answer"),
        other => bail!("{}", other.strip_prefix("err ").unwrap_or(other)),
    }
}
