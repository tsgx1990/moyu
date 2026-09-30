//! moyu-core's single error type.
//!
//! Wraps the error types of every MDK layer moyu-core touches directly, plus
//! moyu's own local-storage / crypto errors. Everything downstream (moyu-cli)
//! only ever needs to match on `MoyuError`.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum MoyuError {
    // -- MDK layers -----------------------------------------------------
    /// `marmot_app::AppError` — the headless-controller (`MarmotApp`/
    /// `AppClient`/`MarmotAppRuntime`) error type. Confirmed public at
    /// `crates/marmot-app/src/error.rs` (re-exported at crate root — see
    /// docs/mdk-api-map.md).
    #[error("marmot-app: {0}")]
    App(#[from] marmot_app::AppError),

    /// `marmot_account::AccountHomeError` — identity/account/secret-store
    /// error type. Confirmed at `crates/marmot-account/src/home.rs` /
    /// `error.rs`.
    #[error("marmot-account: {0}")]
    AccountHome(#[from] marmot_account::AccountHomeError),

    /// `cgka_traits::error::EngineError` — the CGKA/MLS engine error type,
    /// surfaced here only because `crate::engine::describe_key_package`
    /// calls `cgka_engine::key_package::key_package_metadata` directly.
    /// Confirmed path: `crates/traits/CLAUDE.md` lists `src/error.rs` as
    /// owning `EngineError`/`PeelerError`; `marmot-account/src/error.rs:5`
    /// imports it as `use cgka_traits::error::EngineError;`, which is the
    /// path used here too.
    #[error("cgka engine: {0}")]
    Engine(#[from] cgka_traits::error::EngineError),

    // -- moyu's own layers ------------------------------------------------
    // nostr 0.44 has NO unified `nostr::error::Error`; each module owns its
    // own `Error` enum (confirmed against nostr-0.44.4 source: key/mod.rs:34,
    // nips/nip19.rs:59, nips/nip05.rs:21). moyu wraps the ones its `?`
    // operators actually surface.
    /// `nostr::key::Error` — key parsing (`Keys::parse`,
    /// `PublicKey::from_hex`/`from_slice`, `SecretKey::from_slice`).
    #[error("nostr key error: {0}")]
    NostrKey(#[from] nostr::key::Error),

    /// `nostr::nips::nip19::Error` — bech32 (`npub`/`nsec`) encode/decode.
    #[error("nostr bech32 (NIP-19) error: {0}")]
    NostrNip19(#[from] nostr::nips::nip19::Error),

    /// `nostr::nips::nip05::Error` — NIP-05 (`name@domain`) resolution.
    #[error("nostr NIP-05 (resolution) error: {0}")]
    NostrNip05(#[from] nostr::nips::nip05::Error),

    #[error("nostr NIP-05 error: {0}")]
    Nip05(String),

    #[error("passphrase/key-derivation error: {0}")]
    Kdf(String),

    #[error("local encrypted-secret-store error at {path}: {source}")]
    SecretStoreIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("local moyu-state store error: {0}")]
    Store(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json (de)serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("http error resolving NIP-05: {0}")]
    Http(#[from] reqwest::Error),

    #[error("{0}")]
    Other(String),
}

pub type MoyuResult<T> = Result<T, MoyuError>;
