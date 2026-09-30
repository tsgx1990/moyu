//! 全局 relay 配置 `<root>/config.json`。杀「--relay 每条重敲」脚枪。
//! 优先级:CLI --relay(显式一次性覆盖)> config.relays > transport::default_relays()。
//! 只有 init/join 写 config;普通命令的 --relay 是一次性覆盖,不落盘。

use crate::error::MoyuResult;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MoyuConfig {
    #[serde(default = "schema_version")]
    pub v: u32,
    #[serde(default)]
    pub relays: Vec<String>,
}

impl Default for MoyuConfig {
    fn default() -> Self {
        Self {
            v: schema_version(),
            relays: Vec::new(),
        }
    }
}

pub fn config_path(root: &Path) -> PathBuf {
    root.join("config.json")
}

pub fn load(root: &Path) -> MoyuResult<Option<MoyuConfig>> {
    let path = config_path(root);
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub fn save_atomic(root: &Path, cfg: &MoyuConfig) -> MoyuResult<()> {
    fs_private::create_dir_all_private(root)?;
    let bytes = serde_json::to_vec_pretty(cfg)?;
    // temp + rename:避免并发/中断下的 partial file 与丢更新
    let tmp = config_path(root).with_extension("json.tmp");
    fs_private::write_private(&tmp, &bytes)?;
    std::fs::rename(&tmp, config_path(root))?;
    Ok(())
}

/// The relays currently written to `config.json` (empty if the file is absent
/// OR unreadable/corrupt). This is the account's persistent on-disk relay set --
/// what `init`/`join` build up and what `relay list` audits -- as distinct from
/// the process-effective set `resolve_relays` returns (which falls back to the
/// built-in defaults). Degrades to empty rather than erroring so a corrupt
/// config never blocks onboarding (mirrors `resolve_relays`).
pub fn persisted_relays(root: &Path) -> Vec<String> {
    load(root)
        .ok()
        .flatten()
        .map(|c| c.relays)
        .unwrap_or_default()
}

pub fn merge_relays(root: &Path, add: &[String]) -> MoyuResult<Vec<String>> {
    // Start from the persisted set, degrading a corrupt config.json to empty
    // A hand-corrupted config must not block onboarding via
    // `join`/`init`. The unreadable bytes get rewritten with a clean file and
    // the caller's `add` relays are always preserved. (The old
    // `load(root)?` aborted here even though `resolve_relays` degrades.)
    let mut relays = persisted_relays(root);
    for r in add {
        if !relays.iter().any(|x| x == r) {
            relays.push(r.clone());
        }
    }
    save_atomic(
        root,
        &MoyuConfig {
            v: schema_version(),
            relays: relays.clone(),
        },
    )?;
    Ok(relays)
}

/// Remove `url` (exact match) from the persisted relay set, rewriting
/// `config.json`, and return what remains. Lets a user undo a relay that `join`
/// unioned in from an invite code. Forgetting an absent relay
/// is a no-op success; a corrupt config degrades to empty, like `merge_relays`.
pub fn forget_relay(root: &Path, url: &str) -> MoyuResult<Vec<String>> {
    let mut relays = persisted_relays(root);
    relays.retain(|r| r != url);
    save_atomic(
        root,
        &MoyuConfig {
            v: schema_version(),
            relays: relays.clone(),
        },
    )?;
    Ok(relays)
}

pub fn resolve_relays(cli_relays: &[String], root: &Path) -> Vec<String> {
    if !cli_relays.is_empty() {
        return cli_relays.to_vec();
    }
    match load(root) {
        Ok(Some(c)) if !c.relays.is_empty() => c.relays,
        _ => crate::transport::default_relays(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 现有约定:std::env::temp_dir() + 唯一名 + remove_dir_all 清理
    // (keypackage_rotation.rs:98、main.rs:3804),不引 tempfile crate。
    fn tmp_root(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("moyu-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn resolve_precedence() {
        let root = tmp_root("cfg-resolve");
        let root = root.as_path();
        // 无 config、无 CLI → 默认
        assert_eq!(
            resolve_relays(&[], root),
            crate::transport::default_relays()
        );
        // 写 config → config 胜过默认
        save_atomic(
            root,
            &MoyuConfig {
                v: schema_version(),
                relays: vec!["wss://a".into()],
            },
        )
        .unwrap();
        assert_eq!(resolve_relays(&[], root), vec!["wss://a".to_string()]);
        // CLI 覆盖 config(REPLACE,非 merge)
        assert_eq!(
            resolve_relays(&["wss://cli".into()], root),
            vec!["wss://cli".to_string()]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn merge_is_union_no_dupes_order_preserved() {
        let root = tmp_root("cfg-merge");
        let root = root.as_path();
        save_atomic(
            root,
            &MoyuConfig {
                v: schema_version(),
                relays: vec!["wss://a".into(), "wss://b".into()],
            },
        )
        .unwrap();
        let merged = merge_relays(root, &["wss://b".into(), "wss://c".into()]).unwrap();
        assert_eq!(
            merged,
            vec!["wss://a".to_string(), "wss://b".into(), "wss://c".into()]
        );
        assert_eq!(load(root).unwrap().unwrap().relays, merged);
        assert_eq!(load(root).unwrap().unwrap().v, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_absent_is_none() {
        let root = tmp_root("cfg-absent");
        assert_eq!(load(root.as_path()).unwrap(), None);
    }

    #[test]
    fn merge_degrades_on_corrupt_config() {
        let root = tmp_root("cfg-corrupt");
        let root = root.as_path();
        // A valid write creates the dir + file; then corrupt the bytes on disk.
        save_atomic(root, &MoyuConfig::default()).unwrap();
        std::fs::write(config_path(root), b"{ not valid json ][").unwrap();
        // merge must NOT propagate the parse error -- it
        // rewrites a clean config carrying the added relay, so onboarding via
        // `join` is never blocked by a hand-corrupted config.
        let merged = merge_relays(root, &["wss://x".into()]).unwrap();
        assert_eq!(merged, vec!["wss://x".to_string()]);
        assert_eq!(
            load(root).unwrap().unwrap().relays,
            vec!["wss://x".to_string()]
        );
        // persisted_relays also swallows a corrupt read (returns empty).
        std::fs::write(config_path(root), b"garbage").unwrap();
        assert!(persisted_relays(root).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn forget_removes_relay() {
        let root = tmp_root("cfg-forget");
        let root = root.as_path();
        save_atomic(
            root,
            &MoyuConfig {
                v: schema_version(),
                relays: vec!["wss://a".into(), "wss://b".into()],
            },
        )
        .unwrap();
        let left = forget_relay(root, "wss://a").unwrap();
        assert_eq!(left, vec!["wss://b".to_string()]);
        assert_eq!(persisted_relays(root), vec!["wss://b".to_string()]);
        // forgetting an absent relay is a no-op success (idempotent undo).
        let left2 = forget_relay(root, "wss://zzz").unwrap();
        assert_eq!(left2, vec!["wss://b".to_string()]);
        let _ = std::fs::remove_dir_all(root);
    }
}
