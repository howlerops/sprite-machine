//! One banner state for the UI, from the subscription's and the outbox's connectivity
//! (connectivity.rs): offline if either is offline, connecting if either is connecting, else
//! online.

use client_core::Connectivity;

const ALL: [Connectivity; 3] = [
    Connectivity::Connecting,
    Connectivity::Online,
    Connectivity::Offline,
];

#[test]
fn offline_if_either_link_is_offline() {
    for other in ALL {
        assert_eq!(
            Connectivity::Offline.combine(other),
            Connectivity::Offline,
            "subscription offline, outbox {other:?}"
        );
        assert_eq!(
            other.combine(Connectivity::Offline),
            Connectivity::Offline,
            "outbox offline, subscription {other:?}"
        );
    }
}

#[test]
fn connecting_if_either_link_is_connecting_and_none_is_offline() {
    assert_eq!(
        Connectivity::Connecting.combine(Connectivity::Connecting),
        Connectivity::Connecting
    );
    assert_eq!(
        Connectivity::Connecting.combine(Connectivity::Online),
        Connectivity::Connecting
    );
    assert_eq!(
        Connectivity::Online.combine(Connectivity::Connecting),
        Connectivity::Connecting
    );
}

#[test]
fn online_only_when_both_links_are_online() {
    assert_eq!(
        Connectivity::Online.combine(Connectivity::Online),
        Connectivity::Online
    );
}

#[test]
fn the_combined_state_drives_one_banner() {
    let combined = Connectivity::Online.combine(Connectivity::Offline);
    assert_eq!(
        combined.banner(),
        Connectivity::Offline.banner(),
        "one banner text for the combined state"
    );
    assert_eq!(
        Connectivity::Online.combine(Connectivity::Online).banner(),
        None
    );
}
