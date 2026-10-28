//! `wss://` for the tunnel client (tunnel.md §2) against a TLS fake hub whose certificate comes
//! from a CA generated in the test, and the rule that plain `ws://` only reaches loopback.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use protocol::tunnel::Capabilities;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use runner::tunnel::{
    Exit, StaticCredential, Timing, TunnelClient, TunnelCommands, TunnelConfig, TunnelSink, tunnel,
};
use rustls::RootCertStore;
use rustls::ServerConfig;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{Value, json};
use sidecar::store::Store;
use tempfile::TempDir;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL};

const CREDENTIAL: &str = "smm_test_credential";
const WAIT: Duration = Duration::from_secs(5);

// ---- test PKI -------------------------------------------------------------------------------

/// A CA and a leaf certificate for `localhost` / `127.0.0.1`, signed by that CA.
struct Pki {
    ca_root: rustls::pki_types::CertificateDer<'static>,
    leaf: rustls::pki_types::CertificateDer<'static>,
    leaf_key: Vec<u8>,
}

impl Pki {
    fn new() -> Self {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();

        let leaf_key = KeyPair::generate().unwrap();
        let leaf_params =
            CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()]).unwrap();
        let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();
        Self {
            ca_root: ca.der().clone(),
            leaf: leaf.der().clone(),
            leaf_key: leaf_key.serialize_der(),
        }
    }

    fn server_config(&self) -> Arc<ServerConfig> {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.leaf_key.clone()));
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![self.leaf.clone()], key)
                .unwrap();
        Arc::new(config)
    }

    /// A trust store holding only this CA.
    fn trust(&self) -> RootCertStore {
        let mut roots = RootCertStore::empty();
        roots.add(self.ca_root.clone()).unwrap();
        roots
    }
}

// ---- fake hub -------------------------------------------------------------------------------

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

struct Conn {
    ws: WebSocketStream<Box<dyn Io>>,
    authorization: Option<String>,
}

struct Hub {
    addr: SocketAddr,
    conns: mpsc::UnboundedReceiver<Conn>,
    /// Handshakes the hub refused (the client did not trust its certificate).
    tls_refused: Arc<AtomicUsize>,
}

impl Hub {
    /// A hub on loopback. With `tls`, it speaks TLS (wss); without, plain ws.
    async fn start(tls: Option<Arc<ServerConfig>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, conns) = mpsc::unbounded_channel();
        let tls_refused = Arc::new(AtomicUsize::new(0));
        let refused = Arc::clone(&tls_refused);
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let io: Box<dyn Io> = match &tls {
                    Some(config) => match TlsAcceptor::from(Arc::clone(config)).accept(tcp).await {
                        Ok(stream) => Box::new(stream),
                        Err(_) => {
                            refused.fetch_add(1, Ordering::SeqCst);
                            continue;
                        }
                    },
                    None => Box::new(tcp),
                };
                let seen = Arc::new(Mutex::new(None));
                let callback = Upgrade {
                    seen: Arc::clone(&seen),
                };
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(io, callback).await else {
                    continue;
                };
                let authorization = seen.lock().unwrap().clone();
                let _ = tx.send(Conn { ws, authorization });
            }
        });
        Self {
            addr,
            conns,
            tls_refused,
        }
    }

    async fn next_conn(&mut self) -> Conn {
        tokio::time::timeout(WAIT, self.conns.recv())
            .await
            .expect("the runner did not connect")
            .expect("hub stopped")
    }
}

impl Conn {
    async fn recv(&mut self) -> Value {
        match tokio::time::timeout(WAIT, self.ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str(text.as_str()).unwrap(),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    async fn welcome(&mut self) {
        self.ws
            .send(Message::text(
                json!({
                    "t": "welcome", "proto": 1, "resumed": false, "resume_token": "rt_1",
                    "last_up_seq": 0, "heartbeat_ms": 20_000, "max_inflight": 256,
                })
                .to_string(),
            ))
            .await
            .unwrap();
    }
}

/// Records the `Authorization` header and echoes the requested subprotocol.
struct Upgrade {
    seen: Arc<Mutex<Option<String>>>,
}

impl Callback for Upgrade {
    fn on_request(self, req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
        *self.seen.lock().unwrap() = req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if let Some(protocol) = req.headers().get(SEC_WEBSOCKET_PROTOCOL) {
            resp.headers_mut()
                .insert(SEC_WEBSOCKET_PROTOCOL, protocol.clone());
        }
        Ok(resp)
    }
}

// ---- client ---------------------------------------------------------------------------------

/// What a test keeps alive next to the client: the bridge halves must outlive the run.
struct Keep {
    _dir: TempDir,
    _sink: TunnelSink,
    _commands: TunnelCommands,
}

/// A client for `hub_base`; `roots` replaces the webpki roots when given.
fn client(
    hub_base: String,
    roots: Option<RootCertStore>,
) -> (TunnelClient<StaticCredential>, Keep) {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let config = TunnelConfig {
        hub_ws_base: hub_base,
        machine_id: "mch_test".to_owned(),
        runner_version: "0.0.0-test".to_owned(),
        capabilities: Capabilities {
            host_mode: "local".to_owned(),
            harnesses: vec!["acp-mock".to_owned()],
            max_sessions: 1,
            spawn: vec!["same_dir".to_owned()],
            features: Vec::new(),
        },
        timing: Timing {
            backoff_base: Duration::from_millis(5),
            backoff_cap: Duration::from_millis(40),
            stable_after: Duration::from_secs(3600),
            welcome_timeout: WAIT,
        },
    };
    let (client, sink, commands) = tunnel(config, store, StaticCredential::new(CREDENTIAL));
    let client = match roots {
        Some(roots) => client.with_tls_roots(roots),
        None => client,
    };
    let keep = Keep {
        _dir: dir,
        _sink: sink,
        _commands: commands,
    };
    (client, keep)
}

/// Runs `client` in the background; `stop` ends it with a shutdown.
fn spawn_run(
    client: TunnelClient<StaticCredential>,
) -> (oneshot::Sender<()>, JoinHandle<anyhow::Result<Exit>>) {
    let (stop, stopped) = oneshot::channel::<()>();
    let run = tokio::spawn(async move {
        client
            .run(async {
                let _ = stopped.await;
            })
            .await
    });
    (stop, run)
}

async fn stopped(stop: oneshot::Sender<()>, run: JoinHandle<anyhow::Result<Exit>>) -> Exit {
    let _ = stop.send(());
    tokio::time::timeout(WAIT, run)
        .await
        .expect("the client did not stop")
        .unwrap()
        .unwrap()
}

// ---- tests ----------------------------------------------------------------------------------

#[tokio::test]
async fn hello_and_welcome_complete_over_wss_when_the_test_ca_is_trusted() {
    let pki = Pki::new();
    let mut hub = Hub::start(Some(pki.server_config())).await;
    // An https:// base: the client maps it to wss://.
    let base = format!("https://localhost:{}", hub.addr.port());
    let (client, _keep) = client(base, Some(pki.trust()));
    let (stop, run) = spawn_run(client);

    let mut conn = hub.next_conn().await;
    assert_eq!(
        conn.authorization.as_deref(),
        Some("Bearer smm_test_credential"),
        "the credential rides the upgrade over TLS"
    );
    let hello = conn.recv().await;
    assert_eq!(hello["t"], "hello", "hello is the first frame");
    conn.welcome().await;

    assert_eq!(stopped(stop, run).await, Exit::Shutdown);
    let close = conn.recv().await;
    assert_eq!(
        close["t"], "close",
        "shutdown sends the protocol close frame"
    );
    assert_eq!(close["code"], "normal");
}

#[tokio::test]
async fn a_certificate_from_an_untrusted_ca_is_refused() {
    let hub_pki = Pki::new();
    let other_pki = Pki::new();
    let mut hub = Hub::start(Some(hub_pki.server_config())).await;
    let base = format!("wss://localhost:{}", hub.addr.port());
    // The client trusts a different CA than the one that signed the hub's certificate.
    let (client, _keep) = client(base, Some(other_pki.trust()));
    let (stop, run) = spawn_run(client);

    wait_until(|| hub.tls_refused.load(Ordering::SeqCst) >= 2).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), hub.conns.recv())
            .await
            .is_err(),
        "no tunnel connection may be accepted"
    );
    assert_eq!(stopped(stop, run).await, Exit::Shutdown);
}

#[tokio::test]
async fn the_default_webpki_roots_do_not_trust_a_test_ca() {
    let pki = Pki::new();
    let mut hub = Hub::start(Some(pki.server_config())).await;
    let base = format!("wss://localhost:{}", hub.addr.port());
    let (client, _keep) = client(base, None);
    let (stop, run) = spawn_run(client);

    wait_until(|| hub.tls_refused.load(Ordering::SeqCst) >= 2).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), hub.conns.recv())
            .await
            .is_err(),
        "a private CA is not in the production trust store"
    );
    assert_eq!(stopped(stop, run).await, Exit::Shutdown);
}

#[tokio::test]
async fn plain_ws_to_localhost_still_connects() {
    let mut hub = Hub::start(None).await;
    let base = format!("ws://localhost:{}", hub.addr.port());
    let (client, _keep) = client(base, None);
    let (stop, run) = spawn_run(client);

    let mut conn = hub.next_conn().await;
    assert_eq!(conn.recv().await["t"], "hello");
    conn.welcome().await;
    assert_eq!(stopped(stop, run).await, Exit::Shutdown);
}

#[tokio::test]
async fn plain_ws_to_a_remote_host_is_refused_before_connecting() {
    for base in [
        "ws://example.com:8080",
        "ws://192.0.2.10:8080",
        "http://10.1.2.3",
    ] {
        let (client, _keep) = client(base.to_owned(), None);
        let outcome = tokio::time::timeout(WAIT, client.run(std::future::pending::<()>()))
            .await
            .unwrap_or_else(|_| panic!("{base}: the client kept retrying instead of refusing"));
        let error = outcome.expect_err(base).to_string();
        assert!(error.contains("loopback"), "{base}: {error}");
    }
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not reached"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
