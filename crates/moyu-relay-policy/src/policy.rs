//! Pure write-policy decision for a self-hosted Marmot relay. No I/O, no clock — the
//! caller injects `now_secs` and owns `RateState`, so every rule is unit-tested
//! deterministically. The strfry NDJSON shell lives in `main.rs`.

use std::collections::HashMap;

/// The only event kinds moyu's relay accepts (see docs/mdk-api-map.md §3):
/// KeyPackage 30443, group message 445, gift-wrap 1059, NIP-65 relay list
/// 10002, Marmot inbox relay list 10050. Everything else is rejected so the
/// relay can't be abused as a general public Nostr relay.
pub const ALLOWED_KINDS: &[u64] = &[30443, 445, 1059, 10002, 10050];

/// Fixed-window limits. Tune these as real load shows.
///
/// - identity-signed kinds (KeyPackage, relay lists) are limited per pubkey
///   AND per source: a pubkey costs nothing to mint, so the per-pubkey limit
///   alone lets one client publish without bound by rotating keys;
/// - every accepted event, identity or not, also counts against the
///   per-source per-minute limit (group messages and gift wraps are signed by
///   a fresh ephemeral key each, so the source is the only handle on them).
pub const IDENTITY_PER_PUBKEY_PER_HOUR: u32 = 60;
pub const IDENTITY_PER_SOURCE_PER_HOUR: u32 = 600;
pub const EVENTS_PER_SOURCE_PER_MINUTE: u32 = 600;
const HOUR: u64 = 3600;
const MINUTE: u64 = 60;

pub enum Decision {
    Accept,
    Reject(&'static str),
}

/// Hard ceilings on tracked limiter keys, so memory is bounded no matter how
/// many distinct pubkeys or addresses show up. Each table has its own, so
/// filling one cannot lock clients out through another.
///
/// - The two per-source tables refuse an event that needs a NEW entry while
///   they are full of live windows. Filling one takes that many distinct
///   networks active inside one window -- a distributed flood, which a
///   write-policy plugin cannot absorb anyway.
/// - The per-pubkey table does NOT refuse when full: the event is let through
///   untracked. Pubkeys are free, so one network could otherwise fill the
///   table and shut every new identity out for an hour; the per-source
///   identity limit still bounds what that network can publish.
pub const MAX_TRACKED_SOURCES: usize = 50_000;
pub const MAX_TRACKED_PUBKEYS: usize = 100_000;

/// A key is at most this many bytes (a hex pubkey is 64, a normalized address
/// far less), so the ceilings above bound bytes, not just entries.
const MAX_KEY_LEN: usize = 64;

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Allowed,
    /// The key is over its limit for the current window.
    Limited,
    /// The key is new and the table is full of live windows.
    Full,
}

/// One fixed-window limiter: `key -> (window_start_secs, count_in_window)`.
struct Limiter {
    window: u64,
    limit: u32,
    max_keys: usize,
    windows: HashMap<String, (u64, u32)>,
    /// `now` at the last sweep, so a full table does not pay an O(n) scan on
    /// every call.
    last_evict: u64,
}

impl Limiter {
    fn new(window: u64, limit: u32, max_keys: usize) -> Self {
        Self {
            window,
            limit,
            max_keys,
            windows: HashMap::new(),
            last_evict: 0,
        }
    }

    fn evict_expired(&mut self, now: u64) {
        let window = self.window;
        self.windows
            .retain(|_, &mut (start, _)| now < start + window);
    }

    /// Count one event for `key` at `now`. Fixed window: resets when `now`
    /// passes `start + window`.
    fn check(&mut self, key: &str, now: u64) -> Verdict {
        if let Some(entry) = self.windows.get_mut(key) {
            if now >= entry.0 + self.window {
                *entry = (now, 0);
            }
            if entry.1 >= self.limit {
                return Verdict::Limited;
            }
            entry.1 += 1;
            return Verdict::Allowed;
        }
        if self.windows.len() >= self.max_keys {
            if now.saturating_sub(self.last_evict) >= MINUTE {
                self.evict_expired(now);
                self.last_evict = now;
            }
            if self.windows.len() >= self.max_keys {
                return Verdict::Full;
            }
        }
        self.windows.insert(key.to_owned(), (now, 1));
        Verdict::Allowed
    }
}

pub struct RateState {
    /// identity kinds, per pubkey, per hour
    by_pubkey: Limiter,
    /// identity kinds, per source network, per hour
    identity_by_source: Limiter,
    /// every kind, per source, per minute
    by_source: Limiter,
}

impl Default for RateState {
    fn default() -> Self {
        Self {
            by_pubkey: Limiter::new(HOUR, IDENTITY_PER_PUBKEY_PER_HOUR, MAX_TRACKED_PUBKEYS),
            identity_by_source: Limiter::new(
                HOUR,
                IDENTITY_PER_SOURCE_PER_HOUR,
                MAX_TRACKED_SOURCES,
            ),
            by_source: Limiter::new(MINUTE, EVENTS_PER_SOURCE_PER_MINUTE, MAX_TRACKED_SOURCES),
        }
    }
}

impl RateState {
    /// Total tracked keys across the three tables.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.by_pubkey.windows.len()
            + self.identity_by_source.windows.len()
            + self.by_source.windows.len()
    }

    #[cfg(test)]
    fn evict_expired(&mut self, now: u64) {
        self.by_pubkey.evict_expired(now);
        self.identity_by_source.evict_expired(now);
        self.by_source.evict_expired(now);
    }
}

fn is_identity_kind(kind: u64) -> bool {
    matches!(kind, 30443 | 10002 | 10050)
}

/// IPv6 prefix lengths the two per-source limiters group by. One subscriber
/// normally holds at least a /64 and could otherwise get a fresh limit by
/// stepping through it; home connections are commonly handed a /56, so the
/// hourly identity limit -- the one a key-minting client runs into -- groups
/// by that.
const V6_EVENT_PREFIX: u8 = 64;
const V6_IDENTITY_PREFIX: u8 = 56;

/// The limiter key for a client address as strfry reports it. An IPv4
/// address is itself; an IPv6 address is reduced to its first `v6_prefix`
/// bits. Anything that is not an IP address (strfry can be fed other things
/// by a misconfigured proxy header) is kept as text, cut to a bounded length.
pub fn source_key(source: &str, v6_prefix: u8) -> String {
    match source.trim().parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.to_string(),
        Ok(std::net::IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let bits = u128::from(v6);
                let keep = u32::from(v6_prefix.min(128));
                let masked = if keep == 0 {
                    0
                } else {
                    bits & (u128::MAX << (128 - keep))
                };
                format!("{}/{}", std::net::Ipv6Addr::from(masked), keep)
            }
        },
        Err(_) => {
            let mut end = source.len().min(MAX_KEY_LEN);
            while !source.is_char_boundary(end) {
                end -= 1;
            }
            source[..end].to_owned()
        }
    }
}

/// The single decision point. Reject if the kind is not allow-listed or the
/// pubkey is not a pubkey, else apply the rate limits.
pub fn decide(
    kind: u64,
    pubkey: &str,
    source_ip: &str,
    now_secs: u64,
    state: &mut RateState,
) -> Decision {
    const SLOW_DOWN: Decision = Decision::Reject("rate-limited: slow down");
    if !ALLOWED_KINDS.contains(&kind) {
        return Decision::Reject("blocked: this relay only carries moyu (Marmot) traffic");
    }
    if pubkey.len() > MAX_KEY_LEN {
        return Decision::Reject("invalid: pubkey too long");
    }
    let source = source_key(source_ip, V6_EVENT_PREFIX);
    if state.by_source.check(&source, now_secs) != Verdict::Allowed {
        return SLOW_DOWN;
    }
    if is_identity_kind(kind) {
        let network = source_key(source_ip, V6_IDENTITY_PREFIX);
        if state.identity_by_source.check(&network, now_secs) != Verdict::Allowed {
            return SLOW_DOWN;
        }
        // A full pubkey table lets the event through untracked (see
        // `MAX_TRACKED_PUBKEYS`); only a pubkey over its own limit is refused.
        if state.by_pubkey.check(pubkey, now_secs) == Verdict::Limited {
            return SLOW_DOWN;
        }
    }
    Decision::Accept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_the_five_marmot_kinds() {
        for kind in [30443u64, 445, 1059, 10002, 10050] {
            let mut st = RateState::default();
            assert!(
                matches!(decide(kind, "pk", "1.2.3.4", 0, &mut st), Decision::Accept),
                "kind {kind} should be allowed"
            );
        }
    }

    #[test]
    fn rejects_non_allowlisted_kinds() {
        let mut st = RateState::default();
        assert!(matches!(
            decide(1, "pk", "1.2.3.4", 0, &mut st),
            Decision::Reject(_)
        )); // text note
        assert!(matches!(
            decide(0, "pk", "1.2.3.4", 0, &mut st),
            Decision::Reject(_)
        )); // profile
        assert!(matches!(
            decide(4, "pk", "1.2.3.4", 0, &mut st),
            Decision::Reject(_)
        )); // legacy DM
    }

    #[test]
    fn identity_kind_rate_limited_per_pubkey() {
        let mut st = RateState::default();
        // IDENTITY_PER_PUBKEY_PER_HOUR within a 3600s window, keyed by pubkey.
        for _ in 0..IDENTITY_PER_PUBKEY_PER_HOUR {
            assert!(matches!(
                decide(30443, "alice", "1.1.1.1", 10, &mut st),
                Decision::Accept
            ));
        }
        // One more in the same window from the same pubkey is rejected...
        assert!(matches!(
            decide(30443, "alice", "1.1.1.1", 20, &mut st),
            Decision::Reject(_)
        ));
        // ...but a different pubkey is unaffected.
        assert!(matches!(
            decide(30443, "bob", "1.1.1.1", 20, &mut st),
            Decision::Accept
        ));
        // ...and the same pubkey is fine once the window rolls over.
        assert!(matches!(
            decide(30443, "alice", "1.1.1.1", 10 + 3601, &mut st),
            Decision::Accept
        ));
    }

    #[test]
    fn traffic_kind_rate_limited_per_ip_not_pubkey() {
        let mut st = RateState::default();
        // 445/1059 are ephemeral-signed: every event has a fresh pubkey, so the
        // limiter must key on IP. Fill the per-IP window with DISTINCT pubkeys.
        for i in 0..EVENTS_PER_SOURCE_PER_MINUTE {
            let pk = format!("ephemeral-{i}");
            assert!(matches!(
                decide(445, &pk, "9.9.9.9", 100, &mut st),
                Decision::Accept
            ));
        }
        // Same IP, yet another fresh pubkey → rejected (proves it's IP-keyed).
        assert!(matches!(
            decide(445, "ephemeral-new", "9.9.9.9", 100, &mut st),
            Decision::Reject(_)
        ));
        // A different IP is unaffected.
        assert!(matches!(
            decide(445, "ephemeral-x", "8.8.8.8", 100, &mut st),
            Decision::Accept
        ));
    }

    #[test]
    fn evict_expired_drops_only_expired_windows() {
        let mut st = RateState::default();
        // a group message at t=0: one per-source per-minute entry
        assert!(matches!(
            decide(445, "pk", "1.1.1.1", 0, &mut st),
            Decision::Accept
        ));
        // an identity event at t=0 from another source: a per-minute source
        // entry, a per-hour identity-source entry and a per-hour pubkey entry
        assert!(matches!(
            decide(30443, "alice", "2.2.2.2", 0, &mut st),
            Decision::Accept
        ));
        assert_eq!(st.tracked(), 4);
        // advance past the minute windows but not the hour windows
        st.evict_expired(120);
        assert_eq!(
            st.tracked(),
            2,
            "expired per-minute entries evicted, live per-hour entries kept"
        );
    }

    /// The attack the per-pubkey limit alone does not stop: one client
    /// publishing identity events under an endless supply of fresh pubkeys.
    #[test]
    fn identity_kinds_are_also_limited_per_source() {
        let mut st = RateState::default();
        let mut accepted = 0u32;
        for i in 0..(IDENTITY_PER_SOURCE_PER_HOUR + 100) {
            // spread over the hour so the per-minute limit is not what stops it
            let now = u64::from(i) * 5;
            if matches!(
                decide(30443, &format!("fresh-{i}"), "7.7.7.7", now, &mut st),
                Decision::Accept
            ) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, IDENTITY_PER_SOURCE_PER_HOUR);
        // another source is unaffected
        assert!(matches!(
            decide(30443, "someone", "6.6.6.6", 100, &mut st),
            Decision::Accept
        ));
    }

    #[test]
    fn ipv6_is_grouped_by_prefix_and_ipv4_by_address() {
        assert_eq!(source_key("203.0.113.9", 64), "203.0.113.9");
        assert_eq!(source_key(" 203.0.113.9 ", 64), "203.0.113.9");
        assert_eq!(
            source_key("2001:db8:1:2:aaaa:bbbb:cccc:dddd", 64),
            source_key("2001:db8:1:2::1", 64)
        );
        assert_ne!(
            source_key("2001:db8:1:2::1", 64),
            source_key("2001:db8:1:3::1", 64)
        );
        // a /56 holds 256 /64s: 2001:db8:1:200::/64 .. 2001:db8:1:2ff::/64
        assert_eq!(
            source_key("2001:db8:1:200::1", 56),
            source_key("2001:db8:1:2ff::1", 56)
        );
        assert_ne!(
            source_key("2001:db8:1:200::1", 56),
            source_key("2001:db8:1:300::1", 56)
        );
        // the two prefix lengths never produce the same key for one address
        assert_ne!(
            source_key("2001:db8:1:200::1", 56),
            source_key("2001:db8:1:200::1", 64)
        );
        // an IPv4-mapped IPv6 address is that IPv4 address
        assert_eq!(source_key("::ffff:203.0.113.9", 64), "203.0.113.9");
        // not an address: kept, but bounded (and cut on a char boundary)
        assert_eq!(source_key("unix-socket", 64), "unix-socket");
        assert_eq!(source_key(&"é".repeat(100), 64).len(), MAX_KEY_LEN);
        assert!(source_key("2001:db8:ffff:ffff:ffff:ffff:ffff:ffff", 64).len() <= MAX_KEY_LEN);

        // stepping through one /64 does not reset the per-minute limit
        let mut st = RateState::default();
        for i in 0..EVENTS_PER_SOURCE_PER_MINUTE {
            let ip = format!("2001:db8:1:2::{:x}", i + 1);
            assert!(matches!(
                decide(445, "pk", &ip, 100, &mut st),
                Decision::Accept
            ));
        }
        assert!(matches!(
            decide(445, "pk", "2001:db8:1:2:ffff::1", 100, &mut st),
            Decision::Reject(_)
        ));
    }

    /// One home connection (a /56) minting keys across all of its /64s still
    /// hits the hourly identity limit once.
    #[test]
    fn identity_limit_groups_a_whole_ipv6_slash_56() {
        let mut st = RateState::default();
        let mut accepted = 0u32;
        for i in 0..(IDENTITY_PER_SOURCE_PER_HOUR + 50) {
            let ip = format!("2001:db8:1:2{:02x}::1", i % 256);
            let now = u64::from(i) * 5;
            if matches!(
                decide(30443, &format!("fresh-{i}"), &ip, now, &mut st),
                Decision::Accept
            ) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, IDENTITY_PER_SOURCE_PER_HOUR);
    }

    /// A per-source table full of live windows refuses a NEW source, keeps
    /// serving the ones it tracks, and makes room again once windows expire.
    #[test]
    fn a_full_source_table_refuses_new_sources_and_recovers() {
        let mut st = RateState::default();
        for i in 0..MAX_TRACKED_SOURCES {
            st.by_source.windows.insert(
                format!("10.{}.{}.{}", i >> 16, (i >> 8) & 255, i & 255),
                (1_000, 1),
            );
        }
        assert!(matches!(
            decide(445, "pk", "198.51.100.1", 1_001, &mut st),
            Decision::Reject(_)
        ));
        assert_eq!(st.by_source.windows.len(), MAX_TRACKED_SOURCES);
        // an already-tracked source still works
        assert!(matches!(
            decide(445, "pk", "10.0.0.0", 1_001, &mut st),
            Decision::Accept
        ));
        // once the minute windows have expired, the sweep makes room again
        assert!(matches!(
            decide(445, "pk", "198.51.100.1", 1_000 + 61, &mut st),
            Decision::Accept
        ));
        assert!(st.by_source.windows.len() < MAX_TRACKED_SOURCES);
    }

    /// Filling the pubkey table (pubkeys are free) must not lock new
    /// identities out: the event is accepted untracked, the table does not
    /// grow, and the per-source identity limit still applies.
    #[test]
    fn a_full_pubkey_table_does_not_lock_new_identities_out() {
        let mut st = RateState::default();
        for i in 0..MAX_TRACKED_PUBKEYS {
            st.by_pubkey.windows.insert(format!("pk{i}"), (1_000, 1));
        }
        assert!(matches!(
            decide(30443, "a-new-identity", "203.0.113.7", 1_001, &mut st),
            Decision::Accept
        ));
        assert_eq!(st.by_pubkey.windows.len(), MAX_TRACKED_PUBKEYS);
        // a tracked pubkey over its own limit is still refused
        st.by_pubkey
            .windows
            .insert("pk0".to_owned(), (1_000, IDENTITY_PER_PUBKEY_PER_HOUR));
        assert!(matches!(
            decide(30443, "pk0", "203.0.113.8", 1_001, &mut st),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn an_oversized_pubkey_is_rejected_without_being_tracked() {
        let mut st = RateState::default();
        assert!(matches!(
            decide(30443, &"a".repeat(65), "1.2.3.4", 0, &mut st),
            Decision::Reject(_)
        ));
        assert_eq!(st.tracked(), 0);
    }
}
