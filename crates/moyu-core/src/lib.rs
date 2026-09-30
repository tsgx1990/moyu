//! moyu-core: the headless E2EE-chat engine, wrapping MDK (MIT) so that
//! moyu-cli (and, later, a ratatui TUI / any other front end) never has to
//! touch `marmot-app`/`marmot-account`/`cgka-traits` directly.
//!
//! # Status
//!
//! Written against MDK source read directly (`docs/mdk-api-map.md` keeps
//! the file:line citations for every call this crate makes; they date from
//! the first MDK snapshot and are a reading aid, not a contract). The crate
//! compiles, is unit-tested, and is exercised end-to-end against a live
//! relay (`scripts/e2e-local.sh`) on every CI run, so an MDK item named
//! here is known to exist at the path used.
//!
//! # Module map
//!
//! - [`error`] — `MoyuError`, wrapping every MDK + local error type this
//!   crate touches.
//! - [`identity`] — Nostr keypair generation/import, npub/nsec encoding,
//!   NIP-05 resolution, and `Argon2idFileSecretStore` (a custom
//!   `marmot_account::AccountSecretStore` plugging Argon2id + XChaCha20-
//!   Poly1305 nsec-at-rest encryption into `AccountHome`).
//! - [`sqlcipher_kdf`] — the raw Argon2id-KDF + XChaCha20-Poly1305 primitives
//!   `identity::Argon2idFileSecretStore` uses. Read that module's doc comment
//!   first: MDK's own `marmot-app` already HKDF-derives SQLCipher keys for
//!   every MLS/session database automatically, so this module is scoped
//!   narrower than the original design doc assumed.
//! - [`engine`] — `MoyuEngine`, the thin wrapper around `marmot_app::MarmotApp`
//!   that does account setup + exposes a live `AppClient` per account.
//! - [`keypackage_rotation`] — the ~84-day KeyPackage lifecycle policy MDK's
//!   engine does not enforce.
//! - [`transport`] — relay-list helpers (`TransportEndpoint` conversion,
//!   default relay list). Transport wiring itself is automatic inside
//!   `MarmotApp`.
//! - [`store`] — moyu's own local bookkeeping (contacts, active account
//!   label, KeyPackage publish timestamp). NOT the MLS/message-history store
//!   (that's `storage-sqlite`, owned entirely by MDK).
//!
//! (An early `bus` module — a speculative `Command`/`Event` vocabulary for a
//! future dispatcher front end — was deleted once the real thing shipped:
//! the session protocol's vocabulary is `moyu-cli`'s print-free `ops` layer
//! plus its `session::protocol` wire types, which cover workspaces/channels/
//! governance/attachments/pagination the old enums never modeled.)

pub mod config;
pub mod engine;
pub mod error;
pub mod governance;
pub mod identity;
pub mod invite;
pub mod keypackage_rotation;
pub mod sqlcipher_kdf;
pub mod store;
pub mod transport;
pub mod workspace;

pub use error::{MoyuError, MoyuResult};

/// Canonical Marmot inner-app-event `kind`/tag constants
/// (`cgka_traits::app_event`), re-exported so a moyu-cli caller never has to
/// hand-copy a magic number that could drift from upstream. Named explicitly
/// (no glob) rather than re-exporting the whole module: `app_event` also
/// defines ~20 other kind/tag/system-event constants plus the `MarmotAppEvent`/
/// `GroupSystemEvent` codec types moyu doesn't use outside `moyu-core` itself.
/// See `crates/traits/src/app_event.rs` for the authoritative definitions;
/// extend this list (verified via `grep -rn kinds:: crates/moyu-core
/// crates/moyu-cli`) if a caller needs another one.
pub mod kinds {
    pub use cgka_traits::app_event::{
        EVENT_REF_TAG, MARMOT_APP_EVENT_KIND_AGENT_ACTIVITY, MARMOT_APP_EVENT_KIND_AGENT_OPERATION,
        MARMOT_APP_EVENT_KIND_CHAT, MARMOT_APP_EVENT_KIND_GROUP_SYSTEM,
        MARMOT_APP_EVENT_KIND_REACTION, QUOTE_REF_TAG,
    };
}
