//! Nostr identity generation/import + the Argon2id-encrypted secret store
//! that plugs into `marmot_account::AccountHome`.
//!
//! # Confirmed MDK integration point
//!
//! `marmot_account::AccountHome` is opened with a *pluggable* secret-storage
//! backend via a public trait (confirmed at
//! `crates/marmot-account/src/secret_store.rs:60-71`):
//!
//! ```ignore
//! pub trait AccountSecretStore: Send + Sync {
//!     fn has_secret_for_label(&self, label: &str) -> AccountHomeResult<bool>;
//!     fn has_secret_for_account_id(&self, _account_id_hex: &str) -> AccountHomeResult<bool> { Ok(false) }
//!     fn write_secret(&self, account: &AccountSummary, keys: &nostr::Keys) -> AccountHomeResult<()>;
//!     fn load_secret(&self, account: &AccountSummary) -> AccountHomeResult<nostr::Keys>;
//!     fn remove_secret(&self, account: &AccountSummary) -> AccountHomeResult<()>;
//! }
//! ```
//!
//! MDK ships two implementations: `LocalFileSecretStore` (the default used by
//! `AccountHome::open` — writes the nsec as **plaintext JSON**, protected only
//! by a `0600` file mode) and `KeychainSecretStore` (OS keyring). Neither is
//! what moyu wants (the Nostr secret key wrapped with Argon2id(passphrase) →
//! XChaCha20-Poly1305). `AccountHome::open_with_secret_store(root, Arc<dyn
//! AccountSecretStore>)` (confirmed at `home.rs:118-127`) is the documented
//! escape hatch for exactly this — no fork of marmot-account needed.
//!
//! [`Argon2idFileSecretStore`] below is that custom backend.

use std::path::PathBuf;

use marmot_account::{AccountHomeError, AccountHomeResult, AccountSecretStore, AccountSummary};
use nostr::nips::nip19::{FromBech32, ToBech32};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::{MoyuError, MoyuResult};
use crate::sqlcipher_kdf;

/// A generated or imported Nostr identity, ready to hand to
/// `marmot_account::AccountHome`.
pub struct MoyuIdentity {
    pub keys: nostr::Keys,
}

impl MoyuIdentity {
    /// Generate a brand-new identity.
    ///
    /// `nostr::Keys::generate()` — confirmed at nostr 0.44.4
    /// `key/mod.rs:168-171`: gated only by `#[cfg(feature = "std")]` (which
    /// moyu enables) and internally uses `secp256k1::rand::rngs::OsRng`, so it
    /// needs no dedicated nostr RNG feature. This is exactly what
    /// `marmot_account::AccountHome::create_nostr_account` does internally
    /// (`home.rs:142-145`) — moyu-core calls it directly here only so it can
    /// hand the freshly generated `Keys` to the encrypted secret store
    /// *before* `AccountHome` ever sees them (`AccountHome::add_public_account`/
    /// import path always goes through the injected `AccountSecretStore`
    /// either way, so this is equivalent, just made explicit for the `init`
    /// CLI flow which wants to print the npub immediately).
    pub fn generate() -> Self {
        Self {
            keys: nostr::Keys::generate(),
        }
    }

    /// Import an existing identity from an `nsec1..` (bech32) or raw hex
    /// secret key string.
    ///
    /// `nostr::Keys::parse(&str) -> Result<Self, nostr::error::Error>` —
    /// confirmed at `key/mod.rs:113-120`; internally delegates to
    /// `SecretKey::parse`, which accepts either encoding.
    pub fn from_secret_key_str(s: &str) -> MoyuResult<Self> {
        Ok(Self {
            keys: nostr::Keys::parse(s)?,
        })
    }

    /// `npub1...` bech32-encoded public key, for display (`moyu whoami`,
    /// `moyu keypackage publish` confirmation, etc).
    ///
    /// `PublicKey: ToBech32` — confirmed at
    /// `repos/nostr/crates/nostr/src/nips/nip19.rs:289` (`impl ToBech32 for
    /// PublicKey`).
    pub fn npub(&self) -> MoyuResult<String> {
        self.keys
            .public_key()
            .to_bech32()
            .map_err(|e| MoyuError::Other(format!("npub encode failed: {e}")))
    }

    /// `nsec1...` bech32-encoded secret key. Callers must treat the return
    /// value as sensitive (zeroize/drop promptly) — this crate does not wrap
    /// it in `Zeroizing` because `String` is what `ToBech32` returns; prefer
    /// `reveal_nsec_zeroizing` at call sites that can avoid the extra copy.
    ///
    /// `SecretKey: ToBech32` — confirmed at `nip19.rs:233` (`impl ToBech32
    /// for SecretKey`).
    pub fn nsec(&self) -> MoyuResult<Zeroizing<String>> {
        self.keys
            .secret_key()
            .to_bech32()
            .map(Zeroizing::new)
            .map_err(|e| MoyuError::Other(format!("nsec encode failed: {e}")))
    }
}

/// Parse either an `npub1...` bech32 string or a raw 32-byte hex pubkey.
/// `AppClient::create_group`'s `member_refs` accepts both forms directly
/// (confirmed via `MarmotApp::member_key_package`, `marmot-app/src/lib.rs:2266-2311`),
/// so moyu-cli mostly does not need this — it exists for local validation
/// before storing a contact (`crate::store::Contact`).
pub fn parse_npub_or_hex(s: &str) -> MoyuResult<nostr::PublicKey> {
    if let Ok(pk) = nostr::PublicKey::from_bech32(s) {
        return Ok(pk);
    }
    nostr::PublicKey::from_hex(s).map_err(MoyuError::from)
}

/// `marmot_account::AccountSummary.account_id_hex` -> `npub1...`, for display
/// (`moyu whoami`, `moyu init` confirmation). MDK itself has an equivalent
/// helper, `marmot_app::ids::npub_for_account_id`
/// (`crates/marmot-app/src/ids.rs:48-91`, per research) — not re-exported
/// through moyu-core today since pulling in `marmot-app` here only for this
/// one conversion isn't worth the extra coupling; this is a two-line
/// reimplementation using the same confirmed `nostr` bech32 traits as the
/// rest of this module.
pub fn npub_from_hex(account_id_hex: &str) -> MoyuResult<String> {
    let pk = nostr::PublicKey::from_hex(account_id_hex)?;
    pk.to_bech32()
        .map_err(|e| MoyuError::Other(format!("npub encode failed: {e}")))
}

/// Resolve a NIP-05 identifier (`alice@example.com`, or bare `example.com`
/// meaning `_@example.com`) to its Nostr public key.
///
/// This is a `rust-nostr` API (`nostr/src/nips/nip05.rs` in the nostr
/// crate MDK itself pins), not an MDK type: `Nip05Address::parse` builds
/// the well-known-URL
/// (`https://<domain>/.well-known/nostr.json?name=<name>`, `nip05.rs:53-56`);
/// the caller does the HTTP GET (nip05.rs itself is `no_std`-friendly and
/// does no networking); `Nip05Profile::from_raw_json` (`nip05.rs:125-129`)
/// parses the response and extracts + verifies the pubkey for that name.
/// MDK's own crates do not implement NIP-05 resolution anywhere (it is
/// consumer-app territory, not a Marmot/MLS concern), so `moyu-cli`'s
/// `add <npub|nip05>` command needs this moyu-side.
pub async fn resolve_nip05(
    address: &str,
    socks5_proxy: Option<std::net::SocketAddr>,
) -> MoyuResult<nostr::PublicKey> {
    use nostr::nips::nip05::{Nip05Address, Nip05Profile};

    let addr = Nip05Address::parse(address)?;
    // When a SOCKS5 proxy is configured (`--socks5`), route this well-known
    // HTTPS GET through it too -- otherwise a proxy / Tor user leaks
    // the lookup (which identity, which domain, when) over a direct connection
    // even though all relay/MLS traffic is proxied. `socks5h://` = proxy-side
    // DNS (the domain is not resolved locally). `None` = dial directly.
    let response = match socks5_proxy {
        Some(proxy) => {
            reqwest::Client::builder()
                .proxy(reqwest::Proxy::all(format!("socks5h://{proxy}"))?)
                .build()?
                .get(addr.url().to_string())
                .send()
                .await?
        }
        None => reqwest::get(addr.url().to_string()).await?,
    };
    let raw_json = response.text().await?;
    let profile = Nip05Profile::from_raw_json(&addr, &raw_json)?;
    Ok(profile.public_key)
}

/// A peer reference resolved to its canonical string forms: the bech32 `npub`
/// (display, contacts, MDK `member_refs`) and the raw 32-byte `hex` (roster
/// membership compares). Front ends consume this instead of a `nostr` type,
/// keeping the transport crates behind moyu-core.
#[derive(Debug, Clone)]
pub struct ResolvedPeer {
    pub npub: String,
    pub hex: String,
}

/// Resolve a peer reference — `npub1...`, raw hex, or a NIP-05 identifier
/// (`name@domain`) — to [`ResolvedPeer`]. NIP-05 needs an HTTPS GET, routed
/// through the same SOCKS5 proxy as relay traffic when one is set (no
/// direct-connection metadata leak when a proxy is configured; see
/// [`resolve_nip05`]).
pub async fn resolve_peer(
    input: &str,
    socks5_proxy: Option<std::net::SocketAddr>,
) -> MoyuResult<ResolvedPeer> {
    let pk = match parse_npub_or_hex(input) {
        Ok(pk) => pk,
        Err(_) => resolve_nip05(input, socks5_proxy).await?,
    };
    Ok(ResolvedPeer {
        npub: pk
            .to_bech32()
            .map_err(|e| MoyuError::Other(format!("npub encode failed: {e}")))?,
        hex: pk.to_hex(),
    })
}

// ---------------------------------------------------------------------------
// Argon2idFileSecretStore
// ---------------------------------------------------------------------------

/// On-disk shape of an encrypted secret file. One per account label, at
/// `<root>/accounts/<label>/secret.moyu.enc.json` — deliberately a different
/// filename than `LocalFileSecretStore`'s plaintext `secret.json`
/// (`crates/marmot-account/src/secret_store.rs:74-125`) so the two backends
/// never collide if a account dir is ever inspected/migrated by hand.
#[derive(Serialize, Deserialize)]
struct EncryptedSecretFile {
    /// Format version, bump on any incompatible change to this struct.
    version: u8,
    salt_hex: String,
    nonce_hex: String,
    ciphertext_hex: String,
}

/// `marmot_account::AccountSecretStore` impl that Argon2id-derives a key from
/// a user passphrase (held in memory for this store's lifetime — the trait's
/// methods take no passphrase parameter, so it must be supplied once at
/// construction, e.g. prompted at `moyu init`/`moyu whoami` startup) and
/// XChaCha20-Poly1305-encrypts the nsec at rest.
pub struct Argon2idFileSecretStore {
    root: PathBuf,
    passphrase: Zeroizing<Vec<u8>>,
}

impl Argon2idFileSecretStore {
    pub fn new(root: impl Into<PathBuf>, passphrase: impl Into<Vec<u8>>) -> Self {
        Self {
            root: root.into(),
            passphrase: Zeroizing::new(passphrase.into()),
        }
    }

    /// `<root>/accounts/<label>/secret.moyu.enc.json`.
    ///
    /// Invariant: `AccountHome`'s own account directory layout is
    /// `root.join("accounts").join(label)` (`home.rs:133-135`), read from the
    /// same `root` this store is constructed with — moyu-cli must construct
    /// `Argon2idFileSecretStore::new(root, ..)` with the *same* `root` it
    /// passes to `AccountHome::open_with_secret_store(root, ..)` (see
    /// `crate::engine::MoyuEngine::open`). If MDK ever changes this layout,
    /// this path needs to move with it.
    fn secret_path(&self, label: &str) -> PathBuf {
        self.root
            .join("accounts")
            .join(label)
            .join("secret.moyu.enc.json")
    }

    fn account_dir(&self, label: &str) -> PathBuf {
        self.root.join("accounts").join(label)
    }
}

impl AccountSecretStore for Argon2idFileSecretStore {
    fn has_secret_for_label(&self, label: &str) -> AccountHomeResult<bool> {
        Ok(self.secret_path(label).is_file())
    }

    fn write_secret(&self, account: &AccountSummary, keys: &nostr::Keys) -> AccountHomeResult<()> {
        fs_private::create_dir_all_private(&self.account_dir(&account.label))?;

        let salt = sqlcipher_kdf::generate_salt();
        let key = sqlcipher_kdf::derive_key(&self.passphrase, &salt).map_err(|e| {
            AccountHomeError::SecretStore(format!("argon2id derivation failed: {e}"))
        })?;

        // `SecretKey::to_secret_bytes()` — confirmed: this is the exact
        // accessor `marmot-app/src/sqlcipher.rs:253-273` uses internally
        // (`let secret = Zeroizing::new(keys.secret_key().to_secret_bytes());`,
        // per research) to feed its own HKDF derivation, so it is real and
        // stable public API, not a guess.
        let secret_bytes = Zeroizing::new(keys.secret_key().to_secret_bytes());
        let (nonce, ciphertext) = sqlcipher_kdf::encrypt(&key, secret_bytes.as_slice())
            .map_err(|e| AccountHomeError::SecretStore(format!("encrypt failed: {e}")))?;

        let file = EncryptedSecretFile {
            version: 1,
            salt_hex: hex::encode(salt),
            nonce_hex: hex::encode(nonce),
            ciphertext_hex: hex::encode(ciphertext),
        };
        let json = serde_json::to_vec_pretty(&file)?;
        fs_private::write_private(&self.secret_path(&account.label), &json)?;
        Ok(())
    }

    fn load_secret(&self, account: &AccountSummary) -> AccountHomeResult<nostr::Keys> {
        let path = self.secret_path(&account.label);
        let bytes = std::fs::read(&path)
            .map_err(|_| AccountHomeError::SecretNotFound(account.label.clone()))?;
        let file: EncryptedSecretFile = serde_json::from_slice(&bytes)?;

        let salt: [u8; sqlcipher_kdf::SALT_LEN] = hex::decode(&file.salt_hex)?
            .try_into()
            .map_err(|_| AccountHomeError::SecretStore("corrupt salt length".into()))?;
        let key = sqlcipher_kdf::derive_key(&self.passphrase, &salt).map_err(|e| {
            AccountHomeError::SecretStore(format!("argon2id derivation failed: {e}"))
        })?;
        let nonce = hex::decode(&file.nonce_hex)?;
        let ciphertext = hex::decode(&file.ciphertext_hex)?;

        let secret_bytes = sqlcipher_kdf::decrypt(&key, &nonce, &ciphertext)
            .map_err(|_| AccountHomeError::InvalidSecretKey)?; // wrong passphrase surfaces as InvalidSecretKey

        let secret_key = nostr::SecretKey::from_slice(&secret_bytes)
            .map_err(|_| AccountHomeError::InvalidSecretKey)?;
        Ok(nostr::Keys::new(secret_key))
    }

    fn remove_secret(&self, account: &AccountSummary) -> AccountHomeResult<()> {
        let path = self.secret_path(&account.label);
        if path.is_file() {
            // Best-effort zero-overwrite before unlink, mirroring the pattern
            // `LocalFileSecretStore` uses (`secret_store.rs`, per research) —
            // not a strong guarantee on copy-on-write / SSD filesystems, but
            // strictly better than a plain `remove_file`.
            if let Ok(meta) = std::fs::metadata(&path) {
                let zeros = vec![0u8; meta.len() as usize];
                let _ = std::fs::write(&path, &zeros);
            }
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}
