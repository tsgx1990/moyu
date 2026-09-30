//! In-process attachment registries backing the Commands.
//!
//! Two maps behind ONE mutex (never split — a `stage` racing a `take` on
//! separate locks could hand out a token whose path a concurrent take just
//! evicted):
//! - `staged`: opaque `token -> PathBuf` for a file the user just picked (via
//!   the native dialog) or drag-dropped, not yet sent. Consumed exactly once
//!   by `send_attachment` — this is the ONLY channel through which a real
//!   filesystem path crosses from Rust into a session-command call; the
//!   webview only ever sees the `u64` token (see lib.rs's module doc for why
//!   that boundary matters).
//! - `downloads`: `ciphertext_sha256 -> PathBuf` for a file this process has
//!   already fetched via the sidecar's `download` command, so a repeat view
//!   (scroll back, reveal-in-Finder) skips a redundant relay round trip and
//!   Blossom fetch. Keyed on the hash alone, not `(group, hash)`: this
//!   matches the disk cache layer (`attachment_dir` in lib.rs), which is
//!   already pure hash-addressed — a ciphertext sha256 is content-addressed
//!   and globally unique, so `group` adds no disambiguation. `group` also
//!   carries no authorization semantics here (a hash only ever reaches this
//!   process via an event the webview was legitimately handed), so dropping
//!   it from the key is not a confused-deputy risk.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Default)]
struct Inner {
    next_token: u64,
    staged: HashMap<u64, PathBuf>,
    downloads: HashMap<String, PathBuf>,
}

#[derive(Default)]
pub struct Attachments(Mutex<Inner>);

/// Upper bound on concurrently staged (picked/dropped but unsent) paths.
/// Drops the webview ignores (locked screen, reply mode) still mint tokens
/// Rust-side, so without a cap the map grows for the process lifetime;
/// evicting the OLDEST keeps every plausibly-live staging (the composer
/// holds at most one) while bounding the stray ones (merge review, Low).
const MAX_STAGED: usize = 32;

impl Attachments {
    /// Lock the maps, recovering from a poisoned mutex. Every critical section
    /// below leaves `Inner` consistent (single insert/remove/lookup), so a
    /// panic elsewhere while the lock was held must not turn every later
    /// stage/send/download/reveal call into a cascade of panics.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Mint a fresh token for `path` and register it. Never overwrites or
    /// reuses a prior token — each pick/drop gets its own.
    pub fn stage(&self, path: PathBuf) -> u64 {
        let mut inner = self.lock();
        // At most one entry can be over the cap here: every prior `stage()`
        // call left `staged.len() <= MAX_STAGED`, and each call inserts
        // exactly one, so a single eviction always restores the invariant —
        // no loop needed.
        if inner.staged.len() >= MAX_STAGED {
            // Tokens are monotonic, so min key = oldest staging.
            if let Some(oldest) = inner.staged.keys().min().copied() {
                inner.staged.remove(&oldest);
            }
        }
        inner.next_token += 1;
        let token = inner.next_token;
        inner.staged.insert(token, path);
        token
    }

    /// Consume a staged token exactly once. `None` if unknown OR already
    /// consumed (a second `send_attachment` call with the same token — e.g.
    /// a double-click race, or a retry after a failed send — must fail
    /// loudly rather than silently resending stale bytes).
    pub fn take_staged(&self, token: u64) -> Option<PathBuf> {
        self.lock().staged.remove(&token)
    }

    pub fn record_download(&self, hash: String, path: PathBuf) {
        self.lock().downloads.insert(hash, path);
    }

    pub fn download_path(&self, hash: &str) -> Option<PathBuf> {
        self.lock().downloads.get(hash).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn survives_a_poisoned_mutex() {
        let att = Arc::new(Attachments::default());
        let poisoner = Arc::clone(&att);
        // Panic while holding the lock: poisons the mutex.
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.0.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert!(att.0.lock().is_err(), "precondition: mutex is poisoned");
        // Every entry point still works afterwards.
        let token = att.stage(PathBuf::from("/tmp/x"));
        assert_eq!(att.take_staged(token), Some(PathBuf::from("/tmp/x")));
        att.record_download("h".into(), PathBuf::from("/tmp/y"));
        assert_eq!(att.download_path("h"), Some(PathBuf::from("/tmp/y")));
    }
}
