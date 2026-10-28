//! Hub URLs and TLS for the tunnel (tunnel.md §2): `wss://` (or `https://`) is always TLS,
//! verified against the webpki roots; plain `ws://` (or `http://`) is allowed only to loopback.

use std::net::IpAddr;
use std::sync::{Arc, Once};

use rustls::{ClientConfig, RootCertStore};
use tokio_tungstenite::tungstenite::http::Uri;

/// A hub base URL normalised to `ws`/`wss`, and whether it uses TLS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HubUrl {
    pub url: String,
    pub tls: bool,
}

/// Normalises `base` (`wss://`, `https://`, `ws://` or `http://`) and refuses plain `ws://` to
/// any host that is not loopback. Runs before any socket is opened.
pub(crate) fn hub_url(base: &str) -> Result<HubUrl, String> {
    let base = base.trim_end_matches('/');
    let (tls, rest) = if let Some(rest) = base
        .strip_prefix("wss://")
        .or_else(|| base.strip_prefix("https://"))
    {
        (true, rest)
    } else if let Some(rest) = base
        .strip_prefix("ws://")
        .or_else(|| base.strip_prefix("http://"))
    {
        (false, rest)
    } else {
        return Err(format!(
            "hub URL {base} must start with wss://, https://, ws:// or http://"
        ));
    };
    let uri: Uri = format!("ws://{rest}")
        .parse()
        .map_err(|e| format!("hub URL {base}: {e}"))?;
    let host = uri
        .host()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if host.is_empty() {
        return Err(format!("hub URL {base} has no host"));
    }
    if !tls && !is_loopback(host) {
        return Err(format!(
            "refusing plain ws:// to {host}: ws:// is allowed only for loopback hosts, use wss://"
        ));
    }
    let scheme = if tls { "wss" } else { "ws" };
    Ok(HubUrl {
        url: format!("{scheme}://{rest}"),
        tls,
    })
}

/// `localhost`, or an IP address in the loopback range (`127.0.0.0/8`, `::1`).
pub(crate) fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The webpki (Mozilla) root certificates: the production trust store.
pub(crate) fn webpki_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

static PROVIDER: Once = Once::new();

/// A rustls client config (ring provider) that trusts `roots`. The process-wide provider is
/// installed once; a provider installed earlier by someone else is kept.
pub(crate) fn client_config(roots: RootCertStore) -> Arc<ClientConfig> {
    PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wss_and_https_are_tls_and_normalise_to_wss() {
        let hub = hub_url("https://hub.example.com/").unwrap();
        assert_eq!(
            hub,
            HubUrl {
                url: "wss://hub.example.com".into(),
                tls: true
            }
        );
        let hub = hub_url("wss://hub.example.com:8443").unwrap();
        assert_eq!(hub.url, "wss://hub.example.com:8443");
        assert!(hub.tls);
    }

    #[test]
    fn plain_ws_is_allowed_only_to_loopback() {
        for ok in [
            "ws://127.0.0.1:9",
            "ws://localhost:9",
            "ws://[::1]:9",
            "http://127.0.0.2:9",
        ] {
            assert!(!hub_url(ok).unwrap().tls, "{ok}");
        }
        for refused in [
            "ws://example.com",
            "ws://10.0.0.5:8080",
            "ws://[2001:db8::1]:80",
            "ws://0.0.0.0:9",
        ] {
            let error = hub_url(refused).unwrap_err();
            assert!(error.contains("loopback"), "{refused}: {error}");
        }
    }

    #[test]
    fn other_schemes_and_empty_hosts_are_refused() {
        assert!(hub_url("ftp://127.0.0.1").is_err());
        assert!(hub_url("wss://").is_err());
        assert!(hub_url("wss:///path").is_err());
    }
}
