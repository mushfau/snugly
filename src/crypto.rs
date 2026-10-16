//! Password-based encryption.
//!
//! * Key: Argon2id(password, 16-byte random salt, m=64 MiB, t=3, p=1) -> 32-byte master key.
//!   The salt and cost parameters live in the plaintext archive header.
//! * Every block and every index is sealed with XChaCha20-Poly1305 under a subkey derived from
//!   the master key. Each sealed object is `nonce(24) || ciphertext || tag(16)` with a fresh
//!   random 192-bit nonce (safe to pick at random at this size).
//! * The associated data binds each ciphertext to this archive (salt) and to its role and
//!   position (block id / index), so blocks can't be swapped, reordered or replayed from
//!   another archive without detection.
//! * A wrong password simply fails to authenticate the index.

use anyhow::{anyhow, bail, Context, Result};
use chacha20poly1305::aead::{Aead, Generate, KeyInit, Payload};
use chacha20poly1305::XChaCha20Poly1305;
use zeroize::Zeroizing;

pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct KdfParams {
    pub salt: [u8; 16],
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl KdfParams {
    pub fn new_random() -> Self {
        KdfParams { salt: <[u8; 16]>::generate(), m_kib: 64 * 1024, t: 3, p: 1 }
    }
}

pub struct Cipher {
    aead: XChaCha20Poly1305,
    salt: [u8; 16],
}

impl Cipher {
    pub fn from_password(password: &[u8], kdf: &KdfParams) -> Result<Cipher> {
        // Upper bounds stop a hostile header from demanding gigabytes of RAM or minutes of CPU.
        if !(8 * 1024..=1024 * 1024).contains(&kdf.m_kib) || !(1..=16).contains(&kdf.t) || !(1..=16).contains(&kdf.p) {
            bail!("archive has unreasonable key-derivation parameters");
        }
        let params = argon2::Params::new(kdf.m_kib, kdf.t, kdf.p, Some(32)).map_err(|e| anyhow!("argon2: {e}"))?;
        let a2 = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut master = Zeroizing::new([0u8; 32]);
        a2.hash_password_into(password, &kdf.salt, master.as_mut()).map_err(|e| anyhow!("argon2: {e}"))?;
        let sub = Zeroizing::new(blake3::derive_key("snugly v1 2026-10 xchacha20poly1305 data key", master.as_ref()));
        let aead = XChaCha20Poly1305::new_from_slice(sub.as_ref()).map_err(|_| anyhow!("bad key length"))?;
        Ok(Cipher { aead, salt: kdf.salt })
    }

    fn aad(&self, role: &[u8], id: u64) -> Vec<u8> {
        let mut a = Vec::with_capacity(4 + 16 + role.len() + 8);
        a.extend_from_slice(b"snu1");
        a.extend_from_slice(&self.salt);
        a.extend_from_slice(role);
        a.extend_from_slice(&id.to_le_bytes());
        a
    }

    pub fn seal(&self, role: &[u8], id: u64, plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce = chacha20poly1305::XNonce::generate();
        let aad = self.aad(role, id);
        let ct = self
            .aead
            .encrypt(&nonce, Payload { msg: plaintext, aad: &aad })
            .map_err(|_| anyhow!("encryption failed"))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open(&self, role: &[u8], id: u64, sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() < NONCE_LEN + TAG_LEN {
            bail!("sealed data too short");
        }
        let (n, ct) = sealed.split_at(NONCE_LEN);
        let nonce = chacha20poly1305::XNonce::try_from(n).map_err(|_| anyhow!("bad nonce"))?;
        let aad = self.aad(role, id);
        self.aead.decrypt(&nonce, Payload { msg: ct, aad: &aad }).map_err(|_| anyhow!("authentication failed"))
    }
}

/// Password from SNUGLY_PASSWORD (for scripts — visible to other processes of this user, so
/// prefer the prompt) or an interactive no-echo prompt on the terminal.
pub fn read_password(confirm: bool) -> Result<Zeroizing<String>> {
    if let Ok(p) = std::env::var("SNUGLY_PASSWORD") {
        if p.is_empty() {
            bail!("SNUGLY_PASSWORD is empty");
        }
        return Ok(Zeroizing::new(p));
    }
    let p = Zeroizing::new(rpassword::prompt_password("Password: ").context("reading password from terminal")?);
    if p.is_empty() {
        bail!("empty password");
    }
    if confirm {
        if p.chars().count() < 8 {
            eprintln!("warning: passwords shorter than 8 characters are weak");
        }
        let again = Zeroizing::new(rpassword::prompt_password("Repeat password: ")?);
        if *again != *p {
            bail!("passwords do not match");
        }
    }
    Ok(p)
}
