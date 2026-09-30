//! Relay-list config helpers.
//!
//! Deliberately thin. The heavy lifting — constructing a
//! `transport_nostr_adapter::NostrTransportAdapter` backed by a
//! `transport_nostr_adapter::sdk_client::NostrSdkRelayClient` (which wraps a
//! real `nostr_sdk::Client` and handles connect/backoff/publish-fanout/
//! subscribe), plus a `transport_nostr_peeler::NostrMlsPeeler` for kind-445/
//! kind-1059 wrap/peel — all happens automatically *inside*
//! `marmot_app::MarmotApp::with_relays_and_account_home(root, relay_urls,
//! account_home)` (confirmed at `crates/marmot-app/src/lib.rs:951-962`; the
//! `relay_plane` module owns this wiring, per
//! `crates/marmot-app/src/relay_plane/`). moyu-core never needs to touch
//! `transport-nostr-adapter`/`transport-nostr-peeler`/`cgka-session` directly
//! as long as it goes through `MarmotApp`/`AppClient` (see `crate::engine`) —
//! those three crates stay pinned in the workspace root for future direct
//! use (a custom `TransportPeeler` is a plausible M1+ need) but are not
//! moyu-core dependencies today.
//!
//! `transport-nostr-adapter`'s `sdk` feature (which is what provides
//! `NostrSdkRelayClient`, the only production `NostrRelayClient` impl in the
//! crate) is enabled workspace-wide in the root `Cargo.toml` — required for
//! `marmot-app` to have any working relay client at all; confirmed at
//! `crates/transport-nostr-adapter/Cargo.toml:9-11` and `crates/marmot-app/Cargo.toml:43`.

use cgka_traits::TransportEndpoint;

/// A small set of well-known public relays to bootstrap a fresh account with.
/// M0 has no relay-health/Outbox-model logic (a later
/// milestone); these are a starting point, overridable via CLI
/// flags / config, not a hardcoded final list.
///
/// TODO: revisit before any real usage — pick relays known to retain
/// addressable (kind 30000-39999) and ephemeral-ish events long enough for
/// KeyPackage (30443) and ordinary Marmot traffic (445/1059) to be useful,
/// and that are not overloaded. Consider standing up a self-hosted
/// nostr-rs-relay/strfry instead of relying only on
/// public relays.
pub fn default_relays() -> Vec<String> {
    vec![
        "wss://relay.damus.io".to_string(),
        "wss://nos.lol".to_string(),
        "wss://relay.primal.net".to_string(),
    ]
}

/// `cgka_traits::TransportEndpoint(pub String)` — confirmed at
/// `crates/traits/src/transport_adapter.rs:22`, with `From<&str>`/
/// `From<String>` impls (`:36-48`). `AccountSetupRequest.default_relays` /
/// `.bootstrap_relays` (see `crate::engine`) take `Vec<TransportEndpoint>`,
/// not `Vec<String>`, so every relay-URL-list boundary funnels through this.
pub fn to_transport_endpoints(urls: &[String]) -> Vec<TransportEndpoint> {
    urls.iter()
        .map(|u| TransportEndpoint::from(u.as_str()))
        .collect()
}

/// The subset of `relays` that look like loopback endpoints (`127.0.0.0/8`,
/// `::1`, or `localhost`). moyu-cli uses this to refuse a loopback `--relay`
/// unless the user explicitly passes `--dev-allow-loopback` (loopback relays
/// are a local-development/testing convenience only, never a production
/// target), and to name the offending URL(s) in that refusal message.
///
/// This is a *UX guard*, not the security boundary. The authoritative check
/// is MDK's own relay-safety chokepoint
/// (`crates/marmot-app/src/relay_plane/safety.rs`, a full `url::Host` parse),
/// which refuses to open a socket to any non-public relay host at all -- so
/// anything this cheaper parser under-detects still fails closed there. moyu
/// only ever feeds it its own hardcoded `default_relays()` or raw `--relay`
/// CLI strings, so the parse below only has to handle the URL shapes moyu
/// actually produces.
pub fn loopback_relays_in(relays: &[String]) -> Vec<String> {
    relays
        .iter()
        .filter(|r| is_loopback_relay_url(r))
        .cloned()
        .collect()
}

/// Whether any of `relays` is a loopback endpoint. Short-circuiting
/// convenience over [`loopback_relays_in`].
pub fn relays_include_loopback(relays: &[String]) -> bool {
    relays.iter().any(|r| is_loopback_relay_url(r))
}

fn is_loopback_relay_url(url: &str) -> bool {
    // Not a full RFC 3986 parse -- just enough to pull the host out of
    // `ws://[user@]host[:port][/path]` / `wss://[::1]:port` shapes, which is
    // all moyu ever constructs relay URLs from. Under-detection is harmless:
    // MDK's safety.rs does the authoritative host parse and fails closed.
    let rest = match url.find("://") {
        Some(idx) => &url[idx + 3..],
        None => url,
    };
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    // Strip any `user[:pass]@` userinfo prefix so the host, not the userinfo,
    // is what gets classified.
    let host_port = match authority.rsplit_once('@') {
        Some((_userinfo, host)) => host,
        None => authority,
    };
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        // `[::1]:port` -> `::1`
        stripped.split(']').next().unwrap_or(stripped)
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };
    let host = host.to_ascii_lowercase();
    // Only apply the 127.0.0.0/8 (and ::1) loopback-range test to real IP
    // literals. Applying a `starts_with("127.")` string test to a *domain*
    // would misclassify a public host like `127.example.com` as loopback
    // (review finding M-1). A domain is loopback only if it is exactly
    // `localhost`. IPv4-mapped IPv6 (`::ffff:127.0.0.1`) is folded down to its
    // v4 form so it is caught too.
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Ok(ip) => ip.is_loopback(),
        Err(_) => host == "localhost",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_loopback_relay_urls() {
        assert!(relays_include_loopback(
            &["ws://127.0.0.1:7777".to_string()]
        ));
        // Anywhere in 127.0.0.0/8 is loopback.
        assert!(relays_include_loopback(
            &["ws://127.0.0.5:7777".to_string()]
        ));
        assert!(relays_include_loopback(
            &["ws://localhost:7777".to_string()]
        ));
        assert!(relays_include_loopback(&["ws://[::1]:7777".to_string()]));
        assert!(relays_include_loopback(&[
            "wss://relay.damus.io".to_string(),
            "ws://127.0.0.1:7777".to_string(),
        ]));
    }

    #[test]
    fn does_not_flag_public_relay_urls() {
        assert!(!relays_include_loopback(&[
            "wss://relay.damus.io".to_string()
        ]));
        assert!(!relays_include_loopback(&[
            "wss://nos.lol".to_string(),
            "wss://relay.primal.net".to_string()
        ]));
        assert!(!relays_include_loopback(&[]));
    }

    #[test]
    fn does_not_misclassify_public_domains_that_start_with_127() {
        // Review M-1: a public domain like `127.example.com` must NOT be
        // treated as loopback (the old `starts_with("127.")` string test did).
        assert!(!relays_include_loopback(&[
            "wss://127.example.com".to_string()
        ]));
        assert!(!relays_include_loopback(&[
            "wss://127relay.io:443".to_string()
        ]));
    }

    #[test]
    fn loopback_relays_in_returns_the_offending_urls() {
        // The CLI guard names exactly the loopback URLs in its refusal
        // message, so this must return them (and only them), in order.
        let relays = vec![
            "wss://relay.damus.io".to_string(),
            "ws://127.0.0.1:7777".to_string(),
            "wss://nos.lol".to_string(),
            "ws://localhost:7777".to_string(),
        ];
        assert_eq!(
            loopback_relays_in(&relays),
            vec![
                "ws://127.0.0.1:7777".to_string(),
                "ws://localhost:7777".to_string()
            ],
        );
        assert!(loopback_relays_in(&["wss://relay.damus.io".to_string()]).is_empty());
    }

    #[test]
    fn folds_ipv4_mapped_ipv6_loopback() {
        // `::ffff:127.0.0.1` is 127.0.0.1 wearing an IPv6 hat; treat it as
        // loopback so it can't slip past the guard.
        assert!(relays_include_loopback(&[
            "ws://[::ffff:127.0.0.1]:7777".to_string()
        ]));
    }

    #[test]
    fn ignores_userinfo_when_classifying_host() {
        // `user@127.0.0.1` must classify on the host (127.0.0.1), not the
        // whole `user@127.0.0.1` token.
        assert!(relays_include_loopback(&[
            "ws://user@127.0.0.1:7777".to_string()
        ]));
        assert!(relays_include_loopback(&[
            "ws://user:pass@localhost".to_string()
        ]));
        assert!(!relays_include_loopback(&[
            "ws://127.0.0.1@relay.example.com".to_string()
        ]));
    }
}
