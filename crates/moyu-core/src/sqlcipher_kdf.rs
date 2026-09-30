//! Argon2id passphrase-based key derivation + XChaCha20-Poly1305 AEAD helpers.
//!
//! # What this module is actually for (important scope note)
//!
//! An earlier design assumed moyu would need to write its own SQLCipher
//! key-derivation wrapper from scratch, because `storage-sqlite` only accepts
//! a bare `SqlCipherKey` with no derivation logic attached, and the only
//! reference derivation implementation the authors had found at the time was
//! in whitenoise-rs's `sqlcipher.rs` (AGPL, unusable).
//!
//! Reading the real MDK source changes this: `marmot-app` (MIT) has its own
//! `src/sqlcipher.rs` that already does a *complete* HKDF-SHA256 derivation —
//! keyed off the account's Nostr secret key plus a persisted random salt,
//! domain-separated per logical database (session / account-projection /
//! directory-cache) — and applies it automatically to every SQLCipher
//! database `MarmotApp`/`AppClient` opens. See
//! `docs/mdk-api-map.md` for the exact call sites. Because moyu-core wraps
//! `MarmotApp` directly (see `crate::engine`) rather than opening
//! `storage-sqlite` databases itself, **moyu gets that SQLCipher-at-rest
//! protection for free, with zero code here.**
//!
//! What MDK does *not* give moyu for free is protecting the Nostr secret key
//! (`nsec`) itself at rest. MDK's default `AccountSecretStore` impl
//! (`marmot_account::LocalFileSecretStore`) writes the secret key as
//! **plaintext JSON**, relying only on a `0600` file mode
//! (`crates/marmot-account/src/secret_store.rs:74-125`, confirmed by
//! research). That is exactly the gap this module closes: it provides the
//! raw Argon2id-KDF + XChaCha20-Poly1305 primitives that
//! `crate::identity::Argon2idSecretStore` (a custom
//! `marmot_account::AccountSecretStore` impl) uses to encrypt the `nsec` with
//! a user passphrase before it ever touches disk.
//!
//! If a future milestone needs a *second*, moyu-owned SQLCipher database
//! (e.g. app config unrelated to any single Marmot account), the same
//! `derive_key` primitive here is reusable for that too — just derive a
//! 32-byte key and hex-encode it into `storage_sqlite::SqlCipherKey::new(..)`
//! (which accepts anything `Into<String>`, confirmed at
//! `crates/storage-sqlite/src/connection.rs:324-337`).

use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::RngCore;
use rand::rngs::OsRng;
use zeroize::Zeroizing;

use crate::error::{MoyuError, MoyuResult};

/// Salt length for Argon2id. 16 bytes is the libsodium/OWASP-recommended
/// minimum for password hashing salts.
pub const SALT_LEN: usize = 16;
/// XChaCha20-Poly1305 nonce length (extended nonce -> safe to generate
/// randomly per message without a counter).
pub const NONCE_LEN: usize = 24;
/// Derived key length (XChaCha20-Poly1305 key size).
pub const KEY_LEN: usize = 32;

/// Derive a 32-byte symmetric key from a passphrase + salt using Argon2id
/// with library-default parameters (`Argon2::default()` is Argon2id,
/// m=19MiB, t=2, p=1 per the `argon2` crate's `Params::DEFAULT` — reasonable
/// for an interactive CLI unlock, not tuned further for M0).
///
/// TODO: benchmark unlock latency on a
/// representative low-end machine and consider raising `m_cost`
/// (`argon2::Params::new(m_cost, t_cost, p_cost, Some(KEY_LEN))`) if unlock
/// feels too fast to resist offline brute force, or too slow for UX.
pub fn derive_key(
    passphrase: &[u8],
    salt: &[u8; SALT_LEN],
) -> MoyuResult<Zeroizing<[u8; KEY_LEN]>> {
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    Argon2::default()
        .hash_password_into(passphrase, salt, out.as_mut())
        .map_err(|e| MoyuError::Kdf(format!("argon2id derivation failed: {e}")))?;
    Ok(out)
}

/// Generate a fresh random salt suitable for [`derive_key`].
pub fn generate_salt() -> [u8; SALT_LEN] {
    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    salt
}

/// Encrypt `plaintext` under `key` with a fresh random 24-byte nonce.
/// Returns `(nonce, ciphertext)`.
pub fn encrypt(key: &[u8; KEY_LEN], plaintext: &[u8]) -> MoyuResult<(Vec<u8>, Vec<u8>)> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_slice()));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| MoyuError::Kdf(format!("xchacha20poly1305 encrypt failed: {e}")))?;
    Ok((nonce_bytes.to_vec(), ciphertext))
}

/// Decrypt `ciphertext` under `key` and `nonce` (as produced by [`encrypt`]).
pub fn decrypt(
    key: &[u8; KEY_LEN],
    nonce: &[u8],
    ciphertext: &[u8],
) -> MoyuResult<Zeroizing<Vec<u8>>> {
    if nonce.len() != NONCE_LEN {
        return Err(MoyuError::Kdf(format!(
            "expected a {NONCE_LEN}-byte nonce, got {}",
            nonce.len()
        )));
    }
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_slice()));
    let nonce = XNonce::from_slice(nonce);
    let plaintext = cipher.decrypt(nonce, ciphertext).map_err(|e| {
        MoyuError::Kdf(format!(
            "xchacha20poly1305 decrypt failed (wrong passphrase?): {e}"
        ))
    })?;
    Ok(Zeroizing::new(plaintext))
}

#[cfg(test)]
mod tests {
    // NOTE: not run in this environment (no local Rust toolchain, see
    // README). Kept as a written spec of the expected round-trip for the
    // first `cargo test -p moyu-core` once Rust is installed.
    use super::*;

    #[test]
    fn round_trips() {
        let salt = generate_salt();
        let key = derive_key(b"correct horse battery staple", &salt).unwrap();
        let (nonce, ct) = encrypt(&key, b"nsec1exampleexampleexample").unwrap();
        let pt = decrypt(&key, &nonce, &ct).unwrap();
        assert_eq!(&pt[..], b"nsec1exampleexampleexample");
    }

    #[test]
    fn wrong_key_fails() {
        let salt = generate_salt();
        let key = derive_key(b"right passphrase", &salt).unwrap();
        let wrong_key = derive_key(b"wrong passphrase", &salt).unwrap();
        let (nonce, ct) = encrypt(&key, b"secret").unwrap();
        assert!(decrypt(&wrong_key, &nonce, &ct).is_err());
    }
}
