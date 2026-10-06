//! Stored RAR byte mapping, including AES-protected headers and media.
use super::{
    is_media_name,
    remote::{Remote, Source},
};
use anyhow::{bail, ensure, Result};

#[derive(Debug)]
pub(super) struct Span {
    pub file: usize,
    pub offset: u64,
    pub size: u64,
    pub crypto: Option<super::crypto::Crypto>,
    pub plaintext_offset: u64,
    pub cbc_previous: Option<(usize, u64)>,
}

#[derive(Clone)]
pub(super) struct Secret(pub String);
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

#[derive(Debug)]
pub(super) struct Media {
    pub name: String,
    pub spans: Vec<Span>,
    pub expected_size: Option<u64>,
    pub volumes: Vec<usize>,
    pub password: Option<Secret>,
}

impl Media {
    pub fn size(&self) -> u64 {
        self.expected_size
            .unwrap_or_else(|| self.spans.iter().map(|s| s.size).sum())
    }

    pub async fn read<S: Source>(
        &mut self,
        remote: &mut Remote<S>,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        ensure!(
            offset <= self.size() && length as u64 <= self.size() - offset,
            "range outside media"
        );
        let mapped: u64 = self.spans.iter().map(|s| s.size).sum();
        if offset + length as u64 > mapped && !self.volumes.is_empty() {
            // Only inspect continuation headers if MediaInfo needs a range beyond
            // the first volume. Most MKVs stop after the very first article.
            *self = rar(
                remote,
                &self.volumes,
                false,
                self.password.as_ref().map(|p| p.0.as_str()),
            )
            .await?;
        }
        let mut base = 0;
        let mut position = offset;
        let mut output = Vec::with_capacity(length);
        for span in &self.spans {
            if position < base + span.size && output.len() < length {
                let start = position.saturating_sub(base);
                let count = (length - output.len()).min((span.size - start) as usize);
                let bytes = if let Some(cipher) = &span.crypto {
                    let mut cipher = cipher.clone();
                    if span.plaintext_offset + start < 16 {
                        if let Some((file, offset)) = span.cbc_previous {
                            cipher.iv =
                                remote.read(file, offset, 16).await?.as_slice().try_into()?;
                        }
                    }
                    cipher
                        .read(
                            remote,
                            span.file,
                            span.offset,
                            span.plaintext_offset + start,
                            count,
                        )
                        .await?
                } else {
                    remote.read(span.file, span.offset + start, count).await?
                };
                output.extend(bytes);
                position += count as u64;
            }
            base += span.size;
        }
        ensure!(
            output.len() == length,
            "missing archive volume or media range"
        );
        Ok(output)
    }
}

struct Header {
    packed: u64,
    unpacked: Option<u64>,
    name: Option<String>,
    stored: bool,
    encrypted: bool,
    encryption_record: Option<Vec<u8>>,
    salt: Option<[u8; 8]>,
    before: bool,
    after: bool,
    end: bool,
}

// RAR4 field layout and flags: UnRAR headers.hpp / arcread.cpp.
// https://github.com/pmachapman/unrar/blob/master/arcread.cpp
fn rar4(bytes: &[u8]) -> Result<Header> {
    let mut r = Reader::new(bytes);
    r.take(2)?;
    let kind = r.byte()?;
    let flags = r.u16()?;
    let length = r.u16()? as usize;
    ensure!(
        length >= 7 && length <= bytes.len(),
        "invalid RAR4 header size"
    );
    let packed_low = if flags & 0x8000 != 0 {
        r.u32()? as u64
    } else {
        0
    };
    let mut h = Header {
        packed: packed_low,
        unpacked: None,
        name: None,
        stored: false,
        encrypted: kind == 0x73 && flags & 0x80 != 0,
        encryption_record: None,
        salt: None,
        before: flags & 1 != 0,
        after: flags & 2 != 0,
        end: kind == 0x7b,
    };
    if kind == 0x74 {
        ensure!(flags & 0x8000 != 0, "RAR4 file header lacks packed size");
        let mut unpacked = r.u32()? as u64; // Unpacked size.
        r.take(1 + 4 + 4)?; // OS, CRC, time.
        let version = r.byte()?;
        ensure!(
            flags & 4 == 0 || version >= 29,
            "legacy pre-AES RAR encryption is unsupported"
        );
        h.stored = r.byte()? == 0x30;
        let name_length = r.u16()? as usize;
        r.u32()?; // Attributes.
        if flags & 0x100 != 0 {
            h.packed |= (r.u32()? as u64) << 32;
            unpacked |= (r.u32()? as u64) << 32;
        }
        h.unpacked = Some(unpacked);
        let name = r.take(name_length)?;
        ensure!(r.position <= length, "RAR4 name exceeds header");
        h.name = Some(
            String::from_utf8_lossy(name.split(|b| *b == 0).next().unwrap_or_default())
                .into_owned(),
        );
        h.encrypted = flags & 4 != 0;
        if flags & 0x400 != 0 {
            h.salt = Some(r.take(8)?.try_into()?);
        }
        // Directory records cannot be treated as file data.
        if flags & 0xe0 == 0xe0 {
            h.name = None;
        }
    }
    Ok(h)
}

// https://www.rarlab.com/technote.htm : General archive block format,
// File header / Compression information, and File encryption extra record.
fn rar5(bytes: &[u8]) -> Result<Header> {
    let mut r = Reader::new(bytes);
    r.take(4)?;
    let body_length = usize::try_from(r.vint()?)?;
    let length = r
        .position
        .checked_add(body_length)
        .ok_or_else(|| anyhow::anyhow!("RAR5 header overflow"))?;
    ensure!(length <= bytes.len(), "truncated RAR5 header");
    let kind = r.vint()?;
    let flags = r.vint()?;
    let extra = if flags & 1 != 0 {
        usize::try_from(r.vint()?)?
    } else {
        0
    };
    let packed = if flags & 2 != 0 { r.vint()? } else { 0 };
    let mut h = Header {
        packed,
        unpacked: None,
        name: None,
        stored: false,
        encrypted: kind == 4,
        encryption_record: None,
        salt: None,
        before: flags & 8 != 0,
        after: flags & 16 != 0,
        end: kind == 5,
    };
    if kind == 2 {
        let file_flags = r.vint()?;
        let unpacked = r.vint()?; // Unpacked size.
        h.unpacked = (file_flags & 8 == 0).then_some(unpacked);
        r.vint()?; // Attributes.
        if file_flags & 2 != 0 {
            r.take(4)?;
        }
        if file_flags & 4 != 0 {
            r.take(4)?;
        }
        h.stored = r.vint()? & 0x380 == 0;
        r.vint()?; // Host OS.
        let name_length = usize::try_from(r.vint()?)?;
        h.name = Some(String::from_utf8_lossy(r.take(name_length)?).into_owned());
        if file_flags & 1 != 0 {
            h.name = None;
        }
        ensure!(
            extra <= length && r.position <= length - extra,
            "invalid RAR5 extra area"
        );
        let mut extra_reader = Reader::new(&bytes[length - extra..length]);
        while extra_reader.position < extra {
            let size = usize::try_from(extra_reader.vint()?)?;
            let record = extra_reader.take(size)?;
            let mut record_reader = Reader::new(record);
            let kind = record_reader.vint()?;
            h.encrypted |= kind == 1;
            if kind == 1 {
                h.encryption_record = Some(record[record_reader.position..].to_vec());
            }
            ensure!(kind != 5, "RAR redirection entries are not media files");
        }
    }
    if kind == 4 {
        h.encryption_record = Some(bytes[r.position..length].to_vec());
    }
    ensure!(r.position <= length, "RAR5 fields exceed header");
    Ok(h)
}

// Encrypted headers carry either an 8-byte RAR4 salt or a 16-byte RAR5 IV,
// followed by CBC blocks. Header CRCs authenticate the password/decoded layout.
async fn read_header<S: Source>(
    remote: &mut Remote<S>,
    file: usize,
    offset: u64,
    v5: bool,
    header_cipher: Option<&super::crypto::Crypto>,
    v4_encrypted: bool,
    password: Option<&str>,
) -> Result<(Vec<u8>, usize)> {
    let size = remote.size(file).await?;
    let (cipher, prefix) = if v4_encrypted {
        let salt = remote.read(file, offset, 8).await?;
        (
            Some(super::crypto::rar4(Some(salt.as_slice().try_into()?), password).await?),
            8,
        )
    } else if let Some(cipher) = header_cipher {
        let mut cipher = cipher.clone();
        cipher.iv = remote.read(file, offset, 16).await?.as_slice().try_into()?;
        (Some(cipher), 16)
    } else {
        (None, 0)
    };
    let start = offset
        .checked_add(prefix)
        .ok_or_else(|| anyhow::anyhow!("RAR header offset overflow"))?;
    let mut initial = remote
        .read(
            file,
            start,
            if cipher.is_some() {
                16
            } else {
                (size - start).min(16) as usize
            },
        )
        .await?;
    if let Some(cipher) = &cipher {
        cipher.decrypt(&mut initial, cipher.iv)?;
        if !v5 {
            ensure!(
                (0x73..=0x7b).contains(&initial[2]),
                "incorrect archive password or corrupt encrypted RAR header"
            );
        }
    }
    let length = if v5 {
        let mut r = Reader::new(&initial);
        r.take(4)?;
        let len =
            usize::try_from(r.vint()?).map_err(|_| anyhow::anyhow!("invalid RAR5 header size"))?;
        r.position
            .checked_add(len)
            .ok_or_else(|| anyhow::anyhow!("RAR header size overflow"))?
    } else {
        ensure!(initial.len() >= 7, "truncated RAR4 header");
        u16::from_le_bytes([initial[5], initial[6]]) as usize
    };
    ensure!(
        (7..=2 * 1024 * 1024).contains(&length),
        "invalid RAR header size (incorrect password or corrupt archive)"
    );
    let wire_length = if cipher.is_some() {
        length.div_ceil(16) * 16
    } else {
        length
    };
    ensure!(
        wire_length as u64 <= size - start,
        "RAR header exceeds volume (incorrect password or corrupt archive)"
    );
    let mut bytes = remote.read(file, start, wire_length).await?;
    if let Some(cipher) = &cipher {
        cipher.decrypt(&mut bytes, cipher.iv)?;
        bytes.truncate(length);
        let crc_matches = if v5 {
            pesto::yenc::crc32(&bytes[4..]) == u32::from_le_bytes(bytes[..4].try_into()?)
        } else {
            pesto::yenc::crc32(&bytes[2..]) as u16 == u16::from_le_bytes(bytes[..2].try_into()?)
        };
        ensure!(
            crc_matches,
            "incorrect archive password or corrupt encrypted RAR header"
        );
    }
    Ok((bytes, wire_length + prefix as usize))
}

pub(super) async fn rar<S: Source>(
    remote: &mut Remote<S>,
    volumes: &[usize],
    lazy: bool,
    password: Option<&str>,
) -> Result<Media> {
    let mut media: Option<Media> = None;
    for &file in volumes {
        let size = remote.size(file).await?;
        let signature = remote.read(file, 0, 8.min(size as usize)).await?;
        let v5 = signature.starts_with(b"Rar!\x1a\x07\x01\x00");
        ensure!(
            v5 || signature.starts_with(b"Rar!\x1a\x07\x00"),
            "invalid RAR volume signature"
        );
        let mut offset = if v5 { 8 } else { 7 };
        let mut header_crypto = None;
        let mut encrypted_v4_headers = false;
        while offset < size {
            let (bytes, wire_length) = read_header(
                remote,
                file,
                offset,
                v5,
                header_crypto.as_ref(),
                encrypted_v4_headers,
                password,
            )
            .await?;
            let h = if v5 { rar5(&bytes)? } else { rar4(&bytes)? };
            if h.encrypted && h.name.is_none() {
                super::crypto::password(password)?;
                if v5 {
                    header_crypto = Some(
                        super::crypto::rar5(
                            h.encryption_record.as_deref().ok_or_else(|| {
                                anyhow::anyhow!("missing RAR5 encryption parameters")
                            })?,
                            password,
                            false,
                        )
                        .await?,
                    );
                } else {
                    encrypted_v4_headers = true;
                }
                offset += wire_length as u64;
                continue;
            }
            if h.end {
                break;
            }
            let data_offset = offset
                .checked_add(wire_length as u64)
                .ok_or_else(|| anyhow::anyhow!("RAR offset overflow"))?;
            let next = data_offset
                .checked_add(h.packed)
                .ok_or_else(|| anyhow::anyhow!("RAR data size overflow"))?;
            ensure!(next <= size, "RAR member exceeds volume size");
            if let Some(name) = h.name {
                let wanted = media
                    .as_ref()
                    .map_or_else(|| is_media_name(&name), |m| m.name == name);
                if wanted {
                    ensure!(h.stored, "RAR media member is compressed; partial MediaInfo extraction is unavailable");
                    ensure!(
                        h.before == media.is_some(),
                        "missing or out-of-order RAR media volume"
                    );
                    let m = media.get_or_insert_with(|| Media {
                        name,
                        spans: vec![],
                        expected_size: h.unpacked,
                        volumes: volumes.to_vec(),
                        password: password.map(|p| Secret(p.to_owned())),
                    });
                    let cipher = if h.encrypted {
                        if v5 {
                            Some(
                                super::crypto::rar5(
                                    h.encryption_record.as_deref().ok_or_else(|| {
                                        anyhow::anyhow!("missing RAR5 file encryption parameters")
                                    })?,
                                    password,
                                    true,
                                )
                                .await?,
                            )
                        } else {
                            Some(super::crypto::rar4(h.salt, password).await?)
                        }
                    } else {
                        None
                    };
                    let cbc_previous = if h.before && cipher.is_some() {
                        let previous = m
                            .spans
                            .last()
                            .ok_or_else(|| anyhow::anyhow!("missing encrypted RAR continuation"))?;
                        ensure!(
                            previous.crypto.is_some()
                                && previous.size >= 16
                                && previous.size.is_multiple_of(16),
                            "unaligned encrypted RAR volume"
                        );
                        Some((previous.file, previous.offset + previous.size - 16))
                    } else {
                        None
                    };
                    let mapped: u64 = m.spans.iter().map(|s| s.size).sum();
                    let logical_size = if cipher.is_some() && !h.after {
                        m.size()
                            .checked_sub(mapped)
                            .ok_or_else(|| anyhow::anyhow!("invalid encrypted RAR unpacked size"))?
                    } else {
                        h.packed
                    };
                    ensure!(
                        logical_size <= h.packed && h.packed - logical_size < 16,
                        "invalid RAR encrypted padding"
                    );
                    m.spans.push(Span {
                        file,
                        offset: data_offset,
                        size: logical_size,
                        crypto: cipher,
                        plaintext_offset: 0,
                        cbc_previous,
                    });
                    if !h.after {
                        let mut complete = media.expect("inserted above");
                        let mapped: u64 = complete.spans.iter().map(|s| s.size).sum();
                        if let Some(expected) = complete.expected_size {
                            ensure!(
                                mapped == expected,
                                "RAR unpacked size does not match media volumes"
                            );
                        }
                        complete.expected_size = None;
                        complete.volumes.clear();
                        return Ok(complete);
                    }
                    if lazy && m.expected_size.is_some() {
                        ensure!(m.size() >= h.packed, "invalid RAR unpacked size");
                        return Ok(media.expect("inserted above"));
                    }
                    break;
                }
                ensure!(media.is_none(), "RAR continuation member name mismatch");
            }
            offset = next;
        }
    }
    if media.is_some() {
        bail!("missing final RAR media volume");
    }
    bail!("RAR archive does not contain a supported media file")
}

pub(super) struct Reader<'a> {
    bytes: &'a [u8],
    pub position: usize,
}
impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(n)
            .ok_or_else(|| anyhow::anyhow!("header overflow"))?;
        let result = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| anyhow::anyhow!("truncated archive header"))?;
        self.position = end;
        Ok(result)
    }
    pub fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    pub fn vint(&mut self) -> Result<u64> {
        let mut value = 0;
        for shift in (0..=63).step_by(7) {
            let byte = self.byte()?;
            ensure!(shift != 63 || byte <= 1, "RAR vint overflow");
            value |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        bail!("RAR vint overflow")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_truncated_and_overflowing_headers() {
        assert!(rar4(&[0; 6]).is_err());
        assert!(rar5(&[0; 5]).is_err());
        assert!(Reader::new(&[0xff; 10]).vint().is_err());
    }
    #[test]
    fn rar5_compression_and_encryption_flags() {
        // CRC, size, type=file, flags=data, packed size, file flags,
        // unpacked size, attrs, compression=stored, OS, name length, name.
        let bytes = [0, 0, 0, 0, 12, 2, 2, 4, 0, 4, 0, 0, 0, 3, b'a', b'.', b'm'];
        let h = rar5(&bytes).unwrap();
        assert!(h.stored);
        let mut compressed = bytes;
        compressed[11] = 0x80;
        assert!(rar5(&compressed).is_err()); // Truncated two-byte vint.
        assert!(rar5(&[0, 0, 0, 0, 2, 4, 0]).unwrap().encrypted);
    }
}
