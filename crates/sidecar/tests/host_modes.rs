//! Host modes (PLAN-v2 §4.1): `local` is the default, `sandbox` is selected by `SM_HOST`. The
//! two share one tunnel transport and differ in keep-alive, checkpoints, identity and preview.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sidecar::checkpoint::{Checkpoint, CheckpointError};
use sidecar::config::{Config, MachineCredential};
use sidecar::host::identity::{CREDENTIAL_SERVICE, MACHINE_CREDENTIAL_ACCOUNT};
use sidecar::host::{
    BoxFuture, CredentialStore, HostError, HostKind, HostMode, LocalHost, MemoryCredentialStore,
    SandboxHost, from_kind, harness_env,
};
use sidecar::keepalive::{
    DEFAULT_KEEPALIVE_INTERVAL, Hold, KeepAlive, KeepAliveClient, KeepAliveError, NoKeepAlive,
};
use sidecar::preview::{PreviewClient, PreviewError};

/// Builds a config from `SM_*` pairs. `SM_STATE_DIR` is set so parsing never needs `HOME`.
fn config(pairs: &[(&str, &str)]) -> Config {
    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("SM_STATE_DIR".to_owned(), "/var/lib/sm-test".to_owned());
    for (key, value) in pairs {
        vars.insert((*key).to_owned(), (*value).to_owned());
    }
    Config::from_map(&vars).unwrap()
}

/// A keep-alive client that counts pings. Set `fail` to make every ping fail.
#[derive(Default)]
struct CountingClient {
    pings: AtomicUsize,
    fail: bool,
}

impl KeepAliveClient for CountingClient {
    fn ping(&self) -> BoxFuture<'_, Result<(), KeepAliveError>> {
        self.pings.fetch_add(1, Ordering::SeqCst);
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                Err(KeepAliveError::Ping("workspace unreachable".to_owned()))
            } else {
                Ok(())
            }
        })
    }
}

/// A preview client that exposes every port at a fixed URL.
struct FixedPreview;

impl PreviewClient for FixedPreview {
    fn expose(&self, port: u16) -> BoxFuture<'_, Result<String, PreviewError>> {
        Box::pin(async move { Ok(format!("https://preview.example/{port}")) })
    }
}

fn sandbox_config() -> Config {
    config(&[
        ("SM_HUB_URL", "wss://hub.example.com"),
        ("SM_MACHINE_CREDENTIAL", "sandbox-secret"),
    ])
}

fn sandbox_with(client: Arc<CountingClient>) -> SandboxHost {
    SandboxHost::with_clients(&sandbox_config(), client, Arc::new(FixedPreview)).unwrap()
}

#[test]
fn host_kind_defaults_to_local() {
    assert_eq!(HostKind::from_value(None).unwrap(), HostKind::Local);
    assert_eq!(HostKind::from_value(Some("")).unwrap(), HostKind::Local);
    assert_eq!(
        HostKind::from_value(Some("local")).unwrap(),
        HostKind::Local
    );
    assert_eq!(HostKind::default(), HostKind::Local);
}

#[test]
fn sm_host_sandbox_selects_sandbox_and_unknown_values_fail() {
    assert_eq!(
        HostKind::from_value(Some("sandbox")).unwrap(),
        HostKind::Sandbox
    );
    let err = HostKind::from_value(Some("sandbx")).unwrap_err();
    assert_eq!(
        err,
        HostError::UnknownKind {
            value: "sandbx".to_owned()
        }
    );
}

#[test]
fn from_kind_builds_the_selected_mode() {
    let local = from_kind(HostKind::Local, &config(&[])).unwrap();
    assert_eq!(local.kind(), HostKind::Local);

    let sandbox = from_kind(HostKind::Sandbox, &sandbox_config()).unwrap();
    assert_eq!(sandbox.kind(), HostKind::Sandbox);
}

#[test]
fn sandbox_requires_hub_url_and_credential() {
    let no_hub = config(&[("SM_MACHINE_CREDENTIAL", "s")]);
    let err = SandboxHost::new(&no_hub).err().unwrap();
    assert_eq!(err, HostError::MissingHubUrl);

    let no_cred = config(&[("SM_HUB_URL", "wss://hub.example.com")]);
    let err = SandboxHost::new(&no_cred).err().unwrap();
    assert_eq!(err, HostError::MissingCredential);
}

#[test]
fn local_identity_comes_from_the_credential_store() {
    let store = Arc::new(MemoryCredentialStore::new());
    let host = LocalHost::new(&config(&[]), store.clone());
    assert_eq!(host.credential().unwrap(), None);

    store
        .save(
            CREDENTIAL_SERVICE,
            MACHINE_CREDENTIAL_ACCOUNT,
            MachineCredential::new("device-secret"),
        )
        .unwrap();
    let cred = host.credential().unwrap().unwrap();
    assert_eq!(cred.expose(), "device-secret");
}

#[test]
fn both_modes_share_one_tunnel_config() {
    let cfg = config(&[
        ("SM_HUB_URL", "wss://hub.example.com"),
        ("SM_MACHINE_CREDENTIAL", "same-secret"),
    ]);
    let store = Arc::new(MemoryCredentialStore::new());
    store
        .save(
            CREDENTIAL_SERVICE,
            MACHINE_CREDENTIAL_ACCOUNT,
            MachineCredential::new("same-secret"),
        )
        .unwrap();

    let local = LocalHost::new(&cfg, store);
    let sandbox = SandboxHost::new(&cfg).unwrap();

    let local_tunnel = local.tunnel().unwrap();
    let sandbox_tunnel = sandbox.tunnel().unwrap();
    assert_eq!(local_tunnel, sandbox_tunnel);
    assert_eq!(
        local_tunnel.hub_url.as_deref(),
        Some("wss://hub.example.com")
    );
    assert_eq!(
        local_tunnel
            .credential
            .as_ref()
            .map(MachineCredential::expose),
        Some("same-secret")
    );
}

#[test]
fn tunnel_config_debug_never_prints_the_credential() {
    let sandbox = SandboxHost::new(&sandbox_config()).unwrap();
    let debug = format!("{:?}", sandbox.tunnel().unwrap());
    assert!(debug.contains("<redacted>"), "{debug}");
    assert!(!debug.contains("sandbox-secret"), "{debug}");
}

#[test]
fn sandbox_harness_env_removes_the_machine_credential() {
    let sandbox = SandboxHost::new(&sandbox_config()).unwrap();
    let env = vec![
        ("PATH".to_owned(), "/usr/bin".to_owned()),
        (
            "SM_MACHINE_CREDENTIAL".to_owned(),
            "sandbox-secret".to_owned(),
        ),
        ("HOME".to_owned(), "/home/harness".to_owned()),
    ];
    let filtered = sandbox.harness_env(env);
    assert_eq!(
        filtered,
        vec![
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("HOME".to_owned(), "/home/harness".to_owned()),
        ]
    );
    assert!(harness_env(Vec::<(String, String)>::new()).is_empty());
}

#[tokio::test]
async fn local_keep_alive_is_a_no_op() {
    let keep_alive = NoKeepAlive;
    assert_eq!(keep_alive.interval(), None);
    let _guard = keep_alive.hold(Hold::Turn);
    assert!(!keep_alive.tick().await.unwrap());
}

#[tokio::test]
async fn sandbox_pings_only_while_a_turn_or_approval_is_held() {
    let client = Arc::new(CountingClient::default());
    let sandbox = sandbox_with(client.clone());
    let keep_alive = sandbox.keep_alive();
    assert_eq!(keep_alive.interval(), Some(DEFAULT_KEEPALIVE_INTERVAL));

    // Nothing is held: no ping.
    assert!(!keep_alive.tick().await.unwrap());
    assert_eq!(client.pings.load(Ordering::SeqCst), 0);

    // A running turn keeps the sandbox up.
    {
        let _turn = keep_alive.hold(Hold::Turn);
        assert!(keep_alive.tick().await.unwrap());
        assert_eq!(client.pings.load(Ordering::SeqCst), 1);
    }

    // A pending approval also keeps it up, even with no turn.
    {
        let _approval = keep_alive.hold(Hold::Approval);
        assert!(keep_alive.tick().await.unwrap());
        assert_eq!(client.pings.load(Ordering::SeqCst), 2);
    }

    // Released holds stop the pings.
    assert!(!keep_alive.tick().await.unwrap());
    assert_eq!(client.pings.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn sandbox_keep_alive_surfaces_ping_failures() {
    let client = Arc::new(CountingClient {
        pings: AtomicUsize::new(0),
        fail: true,
    });
    let sandbox = sandbox_with(client);
    let _turn = sandbox.keep_alive().hold(Hold::Turn);
    let err = sandbox.keep_alive().tick().await.unwrap_err();
    assert_eq!(
        err,
        KeepAliveError::Ping("workspace unreachable".to_owned())
    );
}

#[tokio::test]
async fn checkpoints_are_stubs_that_name_their_items() {
    let local = LocalHost::new(&config(&[]), Arc::new(MemoryCredentialStore::new()));
    let err = local.checkpointer().snapshot("task-1").await.unwrap_err();
    assert_eq!(
        err,
        CheckpointError::NotImplemented {
            op: "snapshot",
            item: "M2B-011",
        }
    );

    let sandbox = SandboxHost::new(&sandbox_config()).unwrap();
    let err = sandbox.checkpointer().snapshot("task-1").await.unwrap_err();
    assert_eq!(
        err,
        CheckpointError::NotImplemented {
            op: "snapshot",
            item: "M1A-012",
        }
    );
    let restore_err = sandbox
        .checkpointer()
        .restore(&Checkpoint {
            id: "ckpt-1".to_owned(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        restore_err,
        CheckpointError::NotImplemented {
            op: "restore",
            item: "M1A-012"
        }
    ));
}

#[tokio::test]
async fn local_has_no_previews_and_sandbox_delegates_to_its_client() {
    let local = LocalHost::new(&config(&[]), Arc::new(MemoryCredentialStore::new()));
    let err = local.preview().expose(3000).await.unwrap_err();
    assert_eq!(err, PreviewError::NotAvailable);

    let sandbox = sandbox_with(Arc::new(CountingClient::default()));
    let url = sandbox.preview().expose(3000).await.unwrap();
    assert_eq!(url, "https://preview.example/3000");

    // Without an injected client, sandbox previews name the item that wires them.
    let unwired = SandboxHost::new(&sandbox_config()).unwrap();
    let err = unwired.preview().expose(3000).await.unwrap_err();
    assert_eq!(
        err,
        PreviewError::NotImplemented {
            op: "expose",
            item: "M1A-011",
        }
    );
}

#[test]
fn credential_store_is_injected_not_hard_wired() {
    struct Fixed;
    impl CredentialStore for Fixed {
        fn load(
            &self,
            _service: &str,
            _account: &str,
        ) -> Result<Option<MachineCredential>, sidecar::host::CredentialError> {
            Ok(Some(MachineCredential::new("injected")))
        }
        fn save(
            &self,
            _service: &str,
            _account: &str,
            _credential: MachineCredential,
        ) -> Result<(), sidecar::host::CredentialError> {
            Ok(())
        }
    }
    let local = LocalHost::new(&config(&[]), Arc::new(Fixed));
    assert_eq!(local.credential().unwrap().unwrap().expose(), "injected");
}
