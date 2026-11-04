//! Key Derivation Function (KDF) and session key management.

use anyhow::Result;
use argon2::{Algorithm, Argon2, Params, Version};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::fmt;
use zeroize::{Zeroize, Zeroizing};

type HmacSha256 = Hmac<Sha256>;

/// A local cryptographic failure: Argon2id derivation/allocation failure or
/// a poisoned session-cache lock in this process.
///
/// This is never caused by bytes fetched from a provider, so another
/// provider cannot fix it. Callers must abort the job instead of routing it
/// to provider failover (the local counterpart of NZBGet's fatal tier).
/// Carries no key material.
#[derive(Debug)]
pub struct LocalCryptoError {
    message: String,
}

impl LocalCryptoError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for LocalCryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LocalCryptoError {}

pub struct EncryptionSession {
    master_key: Zeroizing<[u8; 32]>,
    control_key: Zeroizing<[u8; 32]>,
    salt: [u8; 16],
}

impl EncryptionSession {
    pub fn new(password: &str, salt: [u8; 16]) -> Result<Self> {
        let params = Params::new(65536, 1, 4, Some(32))
            .map_err(|e| anyhow::anyhow!("invalid argon2 params: {e}"))?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        // KDF intermediates are key material: keep them in `Zeroizing`
        // from creation so no plain stack copy survives the move into
        // the session (which itself stores `Zeroizing` keys).
        let mut master_key = Zeroizing::new([0u8; 32]);
        argon2
            .hash_password_into(password.as_bytes(), &salt, &mut *master_key)
            .map_err(|e| anyhow::anyhow!("argon2 kdf computation failed: {e}"))?;

        let mut mac = HmacSha256::new_from_slice(&*master_key)
            .map_err(|e| anyhow::anyhow!("hmac init error: {e}"))?;
        mac.update(b"yenc-control key");
        // `GenericArray` does not implement `Zeroize` in this build, so
        // copy into a `Zeroizing` array and zero the digest explicitly.
        let mut control_digest = mac.finalize().into_bytes();
        let mut control_key = Zeroizing::new([0u8; 32]);
        control_key.copy_from_slice(&control_digest);
        control_digest.as_mut_slice().zeroize();

        Ok(Self {
            master_key,
            control_key,
            salt,
        })
    }

    pub fn salt(&self) -> [u8; 16] {
        self.salt
    }

    pub(crate) fn master_key(&self) -> &[u8; 32] {
        &self.master_key
    }

    pub(crate) fn control_key(&self) -> &[u8; 32] {
        &self.control_key
    }

    pub(crate) fn derive_body_nonce(&self, segment_index: u32) -> [u8; 24] {
        let mut mac = HmacSha256::new_from_slice(&self.master_key[..]).expect("valid key length");
        mac.update(b"yenc-body nonce");
        mac.update(&segment_index.to_be_bytes());
        let digest = mac.finalize().into_bytes();
        let mut nonce = [0u8; 24];
        nonce.copy_from_slice(&digest[0..24]);
        nonce
    }

    pub(crate) fn derive_control_tweak(&self, segment_index: u32, line_index: u32) -> [u8; 8] {
        let mut mac = HmacSha256::new_from_slice(&self.master_key[..]).expect("valid key length");
        mac.update(b"yenc-control tweak");
        mac.update(&segment_index.to_be_bytes());
        mac.update(&line_index.to_be_bytes());
        let digest = mac.finalize().into_bytes();
        let mut tweak = [0u8; 8];
        tweak.copy_from_slice(&digest[0..8]);
        tweak
    }
}

impl fmt::Debug for EncryptionSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptionSession")
            .field("salt", &self.salt)
            .finish_non_exhaustive()
    }
}
