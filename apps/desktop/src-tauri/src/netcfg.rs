//! Desktop-owned network settings the session supervisor turns into
//! `moyu session` global flags at spawn time.
//!
//! These are the two connectivity knobs the CLI already supports (`--socks5`,
//! `--dev-allow-loopback`, see `moyu-cli` `Cli`) but the GUI previously had no
//! way to set — so a user on a restricted network (public relays unreachable)
//! could never publish their first KeyPackage, and a developer could never
//! point the app at a local relay. Both flags MUST be present at spawn (they
//! govern every relay dial the sidecar makes, including `init`'s publish), so
//! they live in a tiny persisted file the supervisor reads on each (re)spawn.
//!
//! Relay *selection* deliberately does NOT live here: the onboarding relay
//! picker chooses the init relay set (passed per-call) and the sidecar persists
//! it to moyu's own `config.json`. Keeping relays out of this file means one
//! source of truth per concern — no relay split-brain between two configs.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::Manager;

/// The persisted network settings (`net-settings.json`). Both fields default
/// to "off" so a fresh install behaves exactly as before (direct dial, no
/// loopback) until the user opts in via the network-settings panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetSettings {
    /// Schema version of `net-settings.json`. Absent (0.1.x files) reads as
    /// version 1. A file written by a NEWER build is still applied for the
    /// fields this build knows (`socks5`, `allow_loopback`) -- the fail-closed
    /// direction for a proxy setting is to keep using the proxy, never to
    /// silently fall back to a direct connection -- and its version is
    /// preserved on save so a downgrade cannot rewrite it as an older schema.
    #[serde(default = "schema_version")]
    pub v: u32,
    /// SOCKS5 proxy `IP:PORT` (e.g. `"127.0.0.1:1080"`), or `None`/empty for a
    /// direct connection. Stored as a string (not a `SocketAddr`) so a
    /// half-typed value round-trips through the UI without being rejected on
    /// load; it is validated before it can reach a spawn (see [`validated`]).
    #[serde(default)]
    pub socks5: Option<String>,
    /// Pass `--dev-allow-loopback` so a `ws://127.0.0.1:PORT` relay can be used
    /// for local testing. Off by default (loopback relays are refused up front
    /// otherwise).
    #[serde(default)]
    pub allow_loopback: bool,
}

/// Current on-disk schema version (see [`NetSettings::v`]).
pub const SCHEMA_VERSION: u32 = 1;

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for NetSettings {
    fn default() -> Self {
        Self {
            v: SCHEMA_VERSION,
            socks5: None,
            allow_loopback: false,
        }
    }
}

impl NetSettings {
    /// The `moyu` argv these settings produce, always ending in the `session`
    /// subcommand. Global flags go BEFORE the subcommand (the canonical clap
    /// position; `global = true` also accepts them after, but before is
    /// unambiguous). An empty/whitespace `socks5` contributes nothing.
    pub fn session_args(&self) -> Vec<String> {
        let mut argv = Vec::new();
        if let Some(addr) = self.normalized_socks5() {
            argv.push("--socks5".to_string());
            argv.push(addr);
        }
        if self.allow_loopback {
            argv.push("--dev-allow-loopback".to_string());
        }
        argv.push("session".to_string());
        argv
    }

    /// Reject a `socks5` that is not a parseable `IP:PORT` and normalize the
    /// stored value (trim; empty → `None`). This gate matters: an unparseable
    /// `--socks5` makes `moyu` exit on arg-parse, which would send the
    /// supervisor into a respawn loop that never produces a live session — so
    /// a bad value must be caught at save time and surfaced to the user
    /// instead of ever reaching a spawn.
    pub fn validated(self) -> Result<Self, String> {
        let socks5 = match self.normalized_socks5() {
            Some(addr) => {
                addr.parse::<SocketAddr>().map_err(|_| {
                    format!("SOCKS5 代理需为 IP:PORT（如 127.0.0.1:1080，用 IP 而非域名），无法解析:{addr}")
                })?;
                Some(addr)
            }
            None => None,
        };
        Ok(NetSettings {
            // Never downgrade a newer file's version on save (see `v`).
            v: self.v.max(SCHEMA_VERSION),
            socks5,
            allow_loopback: self.allow_loopback,
        })
    }

    /// Trimmed, non-empty `socks5`, or `None`.
    fn normalized_socks5(&self) -> Option<String> {
        self.socks5
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
}

/// `net-settings.json` in the OS app-config dir (same base as the sidecar's
/// data dir on macOS; a sibling file, never colliding).
fn config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("engine: 无法定位配置目录:{e}"))?;
    Ok(dir.join("net-settings.json"))
}

/// Load persisted settings. Any failure falls back to defaults — network
/// settings must never be able to block startup; the worst case is the app
/// behaves as if unconfigured. A *missing* file is the normal unconfigured
/// case (silent); a *present but unparseable* file is logged rather than
/// silently swallowed, since on the restricted network this feature targets a
/// silent revert to direct-dial reads as "the app just can't connect again".
pub fn load(app: &tauri::AppHandle) -> NetSettings {
    let Ok(path) = config_path(app) else {
        return NetSettings::default();
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return NetSettings::default(), // missing / unreadable → unconfigured
    };
    match serde_json::from_slice::<NetSettings>(&bytes) {
        Ok(s) if s.v > SCHEMA_VERSION => {
            // Fail closed: keep the proxy/loopback fields we understand rather
            // than dropping to a direct connection behind the user's back.
            tracing::warn!(
                "net-settings.json is schema v{} but this build understands v{}; \
                 applying the known fields only (upgrade moyu desktop)",
                s.v,
                SCHEMA_VERSION
            );
            s
        }
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "net-settings.json present but unparseable ({e}); \
                 falling back to direct connection"
            );
            NetSettings::default()
        }
    }
}

/// Persist settings (pretty JSON), creating the config dir if needed. Writes to
/// a sibling temp file then renames, so a crash mid-write can't leave a
/// truncated file that [`load`] would read as "unconfigured" and silently drop
/// the user's proxy. The caller MUST pass an already-[`validated`] value.
pub fn save(app: &tauri::AppHandle, settings: &NetSettings) -> Result<(), String> {
    let path = config_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("engine: 无法创建配置目录:{e}"))?;
    }
    let json = serde_json::to_vec_pretty(settings)
        .map_err(|e| format!("engine: 无法序列化网络设置:{e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("engine: 无法写入网络设置:{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("engine: 无法保存网络设置:{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args_are_just_the_subcommand() {
        assert_eq!(NetSettings::default().session_args(), vec!["session"]);
    }

    #[test]
    fn socks5_and_loopback_precede_the_subcommand() {
        let s = NetSettings {
            socks5: Some("127.0.0.1:1080".into()),
            allow_loopback: true,
            ..NetSettings::default()
        };
        assert_eq!(
            s.session_args(),
            vec![
                "--socks5",
                "127.0.0.1:1080",
                "--dev-allow-loopback",
                "session"
            ]
        );
    }

    #[test]
    fn empty_or_whitespace_socks5_is_dropped_from_args() {
        let s = NetSettings {
            socks5: Some("   ".into()),
            allow_loopback: false,
            ..NetSettings::default()
        };
        assert_eq!(s.session_args(), vec!["session"]);
    }

    #[test]
    fn loopback_only() {
        let s = NetSettings {
            socks5: None,
            allow_loopback: true,
            ..NetSettings::default()
        };
        assert_eq!(s.session_args(), vec!["--dev-allow-loopback", "session"]);
    }

    #[test]
    fn validated_accepts_ip_port_and_trims() {
        let s = NetSettings {
            socks5: Some("  127.0.0.1:1080 ".into()),
            allow_loopback: false,
            ..NetSettings::default()
        }
        .validated()
        .expect("valid IP:PORT");
        assert_eq!(s.socks5.as_deref(), Some("127.0.0.1:1080"));
    }

    #[test]
    fn validated_normalizes_empty_to_none() {
        let s = NetSettings {
            socks5: Some("".into()),
            allow_loopback: true,
            ..NetSettings::default()
        }
        .validated()
        .expect("empty is valid (direct)");
        assert_eq!(s.socks5, None);
        assert!(s.allow_loopback);
    }

    #[test]
    fn validated_rejects_hostname_and_bare_ip() {
        // A hostname (not an IP literal) and a port-less IP both fail — the CLI
        // parses `--socks5` as a SocketAddr, so these would crash the sidecar.
        for bad in [
            "localhost:1080",
            "127.0.0.1",
            "not-an-addr",
            "127.0.0.1:99999",
        ] {
            let r = NetSettings {
                socks5: Some(bad.into()),
                allow_loopback: false,
                ..NetSettings::default()
            }
            .validated();
            assert!(r.is_err(), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn schema_version_defaults_for_old_files_and_round_trips() {
        // A 0.1.x file has no `v`: it must parse as schema 1, not fail.
        let old: NetSettings =
            serde_json::from_str(r#"{"socks5":"127.0.0.1:1080","allow_loopback":false}"#).unwrap();
        assert_eq!(old.v, SCHEMA_VERSION);
        assert_eq!(old.socks5.as_deref(), Some("127.0.0.1:1080"));
        // What we write carries the version explicitly.
        let json = serde_json::to_string(&NetSettings::default()).unwrap();
        assert!(json.contains(r#""v":1"#), "{json}");
    }

    #[test]
    fn newer_schema_keeps_known_fields_and_never_downgrades_on_save() {
        // A file from a newer build: the proxy it configured must survive
        // (fail closed), and validating for save must not stamp it as v1.
        let newer: NetSettings =
            serde_json::from_str(r#"{"v":99,"socks5":"127.0.0.1:9050","allow_loopback":false}"#)
                .unwrap();
        assert!(newer.v > SCHEMA_VERSION);
        assert_eq!(
            newer.session_args(),
            vec!["--socks5", "127.0.0.1:9050", "session"]
        );
        let saved = newer.validated().expect("known fields validate");
        assert_eq!(saved.v, 99);
        assert_eq!(saved.socks5.as_deref(), Some("127.0.0.1:9050"));
    }
}
