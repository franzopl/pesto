//! Authenticated body encryption and decryption using XChaCha20-Poly1305.

use anyhow::{ensure, Result};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};

/// Encrypt `data` with XChaCha20-Poly1305 using `key` and `nonce`.
///
/// Returns `(ciphertext, tag)` where `ciphertext.len() == data.len()` and `tag` is 16 bytes.
pub fn encrypt_body(data: &[u8], key: &[u8; 32], nonce: &[u8; 24]) -> Result<(Vec<u8>, [u8; 16])> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut ct_and_tag = cipher
        .encrypt(XNonce::from_slice(nonce), data)
        .map_err(|e| anyhow::anyhow!("aead encrypt failed: {e}"))?;
    ensure!(
        ct_and_tag.len() >= 16,
        "ciphertext too short for authentication tag"
    );
    let tag_bytes = ct_and_tag.split_off(ct_and_tag.len() - 16);
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&tag_bytes);
    Ok((ct_and_tag, tag))
}

/// Authenticate and decrypt `ciphertext` and `tag` with XChaCha20-Poly1305 using `key` and `nonce`.
///
/// Returns decrypted plaintext on success.
/// Enforces the Zero-Output Guarantee: on authentication failure or error, releases no plaintext.
pub fn decrypt_body(
    ciphertext: &[u8],
    tag: &[u8; 16],
    key: &[u8; 32],
    nonce: &[u8; 24],
) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut ct_and_tag = Vec::with_capacity(ciphertext.len() + 16);
    ct_and_tag.extend_from_slice(ciphertext);
    ct_and_tag.extend_from_slice(tag);
    cipher
        .decrypt(XNonce::from_slice(nonce), ct_and_tag.as_ref())
        .map_err(|_| anyhow::anyhow!("Poly1305 authentication failed"))
}
