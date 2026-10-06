//! Archive-specific key derivation and AES-CBC range decryption.
//! Keys/passwords are never included in Debug output or error messages.
use super::{
    archive::Reader,
    remote::{Remote, Source},
};
use aes::{
    cipher::{Array, BlockCipherDecrypt, KeyInit},
    Aes128, Aes256,
};
use anyhow::{ensure, Context, Result};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub(super) struct Crypto {
    key: Vec<u8>,
    pub iv: [u8; 16],
}
impl std::fmt::Debug for Crypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AES-CBC([redacted])")
    }
}
impl Crypto {
    pub fn decrypt(&self, bytes: &mut [u8], iv: [u8; 16]) -> Result<()> {
        ensure!(
            bytes.len().is_multiple_of(16),
            "unaligned encrypted archive data"
        );
        match self.key.len() {
            16 => decrypt_blocks(Aes128::new_from_slice(&self.key)?, bytes, iv),
            32 => decrypt_blocks(Aes256::new_from_slice(&self.key)?, bytes, iv),
            _ => anyhow::bail!("unsupported archive encryption key length"),
        }
        Ok(())
    }

    pub async fn read<S: Source>(
        &self,
        remote: &mut Remote<S>,
        file: usize,
        offset: u64,
        plaintext_offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        if length == 0 {
            return Ok(vec![]);
        }
        let start = plaintext_offset / 16 * 16;
        let skip = (plaintext_offset - start) as usize;
        let count = (skip + length).div_ceil(16) * 16;
        let before = if start == 0 { 0 } else { 16 };
        let position = offset
            .checked_add(start - before)
            .context("encrypted range overflow")?;
        let mut bytes = remote.read(file, position, count + before as usize).await?;
        let iv = if before == 0 {
            self.iv
        } else {
            bytes[..16].try_into()?
        };
        let data = &mut bytes[before as usize..];
        self.decrypt(data, iv)?;
        Ok(data[skip..skip + length].to_vec())
    }
}

fn decrypt_blocks<C: BlockCipherDecrypt<BlockSize = aes::cipher::consts::U16>>(
    cipher: C,
    bytes: &mut [u8],
    mut iv: [u8; 16],
) {
    for chunk in bytes.as_chunks_mut::<16>().0 {
        let mut ciphertext = [0; 16];
        ciphertext.copy_from_slice(chunk);
        let mut block = Array::from(ciphertext);
        cipher.decrypt_block(&mut block);
        for i in 0..16 {
            chunk[i] = block[i] ^ iv[i];
        }
        iv = ciphertext;
    }
}

pub(super) fn password(value: Option<&str>) -> Result<&str> {
    value.context("archive requires a password; use --password or the NZB password metadata")
}

// RAR5 uses PBKDF2-HMAC-SHA256 on UTF-8, with supplementary password
// verification at iterations+32. https://github.com/pmachapman/unrar/blob/master/crypt5.cpp
pub(super) async fn rar5(record: &[u8], value: Option<&str>, file_record: bool) -> Result<Crypto> {
    let mut r = Reader::new(record);
    ensure!(r.vint()? == 0, "unsupported RAR5 encryption version");
    let flags = r.vint()?;
    let power = r.byte()?;
    ensure!(
        power <= 20,
        "RAR5 password derivation exceeds the partial inspection CPU limit"
    );
    let salt: [u8; 16] = r.take(16)?.try_into()?;
    let iv = if file_record {
        r.take(16)?.try_into()?
    } else {
        [0; 16]
    };
    let check: Option<[u8; 12]> = if flags & 1 != 0 {
        Some(r.take(12)?.try_into()?)
    } else {
        None
    };
    let value = password(value)?.to_owned();
    tokio::task::spawn_blocking(move || {
        let base = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(value.as_bytes())?;
        let mut first = base.clone();
        first.update(&salt);
        first.update(&[0, 0, 0, 1]);
        let mut u: [u8; 32] = first.finalize().into_bytes().into();
        let mut accumulated = u;
        let count = 1u32 << power;
        let mut key = [0; 32];
        for iteration in 1..=count + 32 {
            if iteration == count {
                key = accumulated;
            }
            if iteration == count + 32 {
                break;
            }
            let mut mac = base.clone();
            mac.update(&u);
            u = mac.finalize().into_bytes().into();
            for (a, b) in accumulated.iter_mut().zip(u) {
                *a ^= b;
            }
        }
        if let Some(check) = check {
            ensure!(
                Sha256::digest(&check[..8])[..4] == check[8..],
                "corrupt RAR password check data"
            );
            let mut derived = [0; 8];
            for (i, b) in accumulated.iter().enumerate() {
                derived[i % 8] ^= b;
            }
            ensure!(
                derived
                    .iter()
                    .zip(&check[..8])
                    .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                    == 0,
                "incorrect archive password"
            );
        }
        Ok(Crypto {
            key: key.to_vec(),
            iv,
        })
    })
    .await
    .context("RAR5 key derivation task failed")?
}

// RAR4 (RAR 3.x AES scheme): UTF-16LE + salt, 0x40000 SHA1 rounds.
// The legacy in-place message schedule mutation for long passwords is part
// of compatibility, not standard SHA1. See UnRAR crypt3.cpp and sha1.cpp.
pub(super) async fn rar4(salt: Option<[u8; 8]>, value: Option<&str>) -> Result<Crypto> {
    let value = password(value)?.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut raw: Vec<u8> = value.encode_utf16().flat_map(u16::to_le_bytes).collect();
        ensure!(raw.len() <= 254, "RAR4 password is too long");
        if let Some(salt) = salt {
            raw.extend(salt);
        }
        let mut hash = Sha1::new();
        let mut iv = [0; 16];
        let mut processed = 0usize;
        for i in 0..0x40000u32 {
            hash.update(&raw);
            let mut start = 64 - (processed % 64);
            while start + 64 <= raw.len() {
                let block = &mut raw[start..start + 64];
                let mut words = [0u32; 80];
                for (j, b) in block.as_chunks::<4>().0.iter().enumerate() {
                    words[j] = u32::from_be_bytes(*b);
                }
                for j in 16..80 {
                    words[j] = (words[j - 3] ^ words[j - 8] ^ words[j - 14] ^ words[j - 16])
                        .rotate_left(1);
                }
                for (b, word) in block.as_chunks_mut::<4>().0.iter_mut().zip(&words[64..]) {
                    b.copy_from_slice(&word.to_le_bytes());
                }
                start += 64;
            }
            processed += raw.len();
            hash.update(&i.to_le_bytes()[..3]);
            processed += 3;
            if i % 0x4000 == 0 {
                iv[(i / 0x4000) as usize] = hash.clone().finalize()[19];
            }
        }
        let digest = hash.finalize();
        let mut key = Vec::with_capacity(16);
        for b in digest[..16].as_chunks::<4>().0 {
            key.extend(b.iter().rev());
        }
        Ok(Crypto { key, iv })
    })
    .await
    .context("RAR4 key derivation task failed")?
}

// 7zAES hashes the concatenated salt, UTF-16LE input and LE64 counter with
// SHA256, repeated 2^power times.
// https://github.com/ip7z/7zip/blob/main/CPP/7zip/Crypto/7zAes.cpp
pub(super) async fn sevenz(properties: &[u8], value: Option<&str>) -> Result<Crypto> {
    ensure!(!properties.is_empty(), "missing 7z encryption properties");
    let first = properties[0];
    let power = first & 0x3f;
    ensure!(
        power <= 20 || power == 0x3f,
        "7z password derivation exceeds the partial inspection CPU limit"
    );
    let (salt, iv) = if first & 0xc0 == 0 {
        (vec![], [0; 16])
    } else {
        ensure!(properties.len() >= 2, "truncated 7z encryption properties");
        let salt_size = ((first >> 7) & 1) as usize + (properties[1] >> 4) as usize;
        let iv_size = ((first >> 6) & 1) as usize + (properties[1] & 15) as usize;
        ensure!(
            iv_size <= 16 && properties.len() == 2 + salt_size + iv_size,
            "invalid 7z encryption properties"
        );
        let mut iv = [0; 16];
        iv[..iv_size].copy_from_slice(&properties[2 + salt_size..]);
        (properties[2..2 + salt_size].to_vec(), iv)
    };
    let value = password(value)?.to_owned();
    tokio::task::spawn_blocking(move || {
        let raw: Vec<u8> = value.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let key = if power == 0x3f {
            let mut key = [0; 32];
            for (slot, b) in key.iter_mut().zip(salt.iter().chain(&raw)) {
                *slot = *b;
            }
            key
        } else {
            let mut hash = Sha256::new();
            for i in 0..1u64 << power {
                hash.update(&salt);
                hash.update(&raw);
                hash.update(i.to_le_bytes());
            }
            hash.finalize().into()
        };
        Ok(Crypto {
            key: key.to_vec(),
            iv,
        })
    })
    .await
    .context("7z key derivation task failed")?
}
