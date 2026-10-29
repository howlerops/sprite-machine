//! Deep links (hub `links` module, M2-010): `https://<hub>/t/<thread_id>`.

use protocol::turn_event::is_valid_id;
use url::Url;

/// The thread id of a hub deep link, or `None` for anything else.
///
/// Accepted: the same scheme, host and port as `hub`, the path exactly `/t/<id>` (one trailing
/// slash allowed) with a valid id; a query or fragment is ignored. Rejected: other hosts or ports,
/// other schemes, user info, other or longer paths, ids with odd characters (including encoded
/// ones). The app defines no custom URL scheme yet, so only hub links parse.
pub fn hub_link_thread(link: &str, hub: &str) -> Option<String> {
    let link = Url::parse(link.trim()).ok()?;
    let hub = Url::parse(hub.trim()).ok()?;
    if !matches!(link.scheme(), "http" | "https")
        || link.scheme() != hub.scheme()
        || link.host_str()? != hub.host_str()?
        || link.port_or_known_default() != hub.port_or_known_default()
        || !link.username().is_empty()
        || link.password().is_some()
    {
        return None;
    }
    let path = link.path();
    let path = path.strip_suffix('/').unwrap_or(path);
    let id = path.strip_prefix("/t/")?;
    // `%` is not a valid id character, so an encoded slash or dot segment fails here.
    is_valid_id(id).then(|| id.to_owned())
}
