//! Authenticated body encryption and decryption using XChaCha20-Poly1305.

use anyhow::{ensure, Result};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};

/// Format the exact AAD serialization for yEnc body encryption
/// (yEnc Body Encryption Standard v1.2 §2.1).
///
/// Serialization layout (concatenation, big-endian integers):
/// - 0..13: ASCII "yenc-body aad" (13 bytes)
/// - 13..17: uint32_be(partNumber)
/// - 17..21: uint32_be(partBegin)
/// - 21..29: uint64_be(segmentBytes)
/// - 29..33: uint32_be(nameLength)
/// - 33..33+nameLength: nameBytes (exact =ybegin name parameter value)
pub fn format_body_aad(
    part_number: u32,
    part_begin: u32,
    segment_bytes: u64,
    name: &[u8],
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(33 + name.len());
    aad.extend_from_slice(b"yenc-body aad");
    aad.extend_from_slice(&part_number.to_be_bytes());
    aad.extend_from_slice(&part_begin.to_be_bytes());
    aad.extend_from_slice(&segment_bytes.to_be_bytes());
    aad.extend_from_slice(&(name.len() as u32).to_be_bytes());
    aad.extend_from_slice(name);
    aad
}

/// Encrypt `data` with XChaCha20-Poly1305 using `key`, `nonce`, and associated data `aad`.
///
/// Returns `(ciphertext, tag)` where `ciphertext.len() == data.len()` and `tag` is 16 bytes.
pub fn encrypt_body(
    data: &[u8],
    aad: &[u8],
    key: &[u8; 32],
    nonce: &[u8; 24],
) -> Result<(Vec<u8>, [u8; 16])> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let payload = Payload { msg: data, aad };
    let mut ct_and_tag = cipher
        .encrypt(XNonce::from_slice(nonce), payload)
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

/// Authenticate and decrypt `ciphertext` and `tag` with XChaCha20-Poly1305 using `key`, `nonce`, and `aad`.
///
/// Returns decrypted plaintext on success.
/// Enforces the Zero-Output Guarantee: on authentication failure or error, releases no plaintext.
pub fn decrypt_body(
    ciphertext: &[u8],
    tag: &[u8; 16],
    aad: &[u8],
    key: &[u8; 32],
    nonce: &[u8; 24],
) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut ct_and_tag = Vec::with_capacity(ciphertext.len() + 16);
    ct_and_tag.extend_from_slice(ciphertext);
    ct_and_tag.extend_from_slice(tag);
    let payload = Payload {
        msg: &ct_and_tag,
        aad,
    };
    cipher
        .decrypt(XNonce::from_slice(nonce), payload)
        .map_err(|_| anyhow::anyhow!("Poly1305 authentication failed"))
}
