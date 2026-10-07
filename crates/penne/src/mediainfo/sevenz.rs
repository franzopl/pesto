//! Parse 7z metadata with the maintained 7z decoder, then map stored media
//! directly onto Copy/AES streams. Compressed payloads remain unsupported.
use super::{
    archive::{Media, Span},
    crypto, is_media_name,
};
use crate::remote::{Remote, Source};
use anyhow::{ensure, Context, Result};
use sevenz_rust2::{Archive, Password};
use std::{
    collections::BTreeMap,
    io::{self, Read, Seek, SeekFrom},
};

/// A synchronous parser can request an uncached range but never receives
/// fabricated zero bytes. The async caller fetches it and retries parsing.
struct HeaderReader {
    size: u64,
    position: u64,
    chunks: BTreeMap<u64, Vec<u8>>,
    missing: Option<(u64, usize)>,
}
impl Read for HeaderReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.position >= self.size {
            return Ok(0);
        }
        if let Some((&start, data)) = self.chunks.range(..=self.position).next_back() {
            let index = (self.position - start) as usize;
            if index < data.len() {
                let count = buf.len().min(data.len() - index);
                buf[..count].copy_from_slice(&data[index..index + count]);
                self.position += count as u64;
                return Ok(count);
            }
        }
        self.missing = Some((
            self.position,
            (self.size - self.position).min(64 * 1024) as usize,
        ));
        Err(io::Error::from(io::ErrorKind::WouldBlock))
    }
}
impl Seek for HeaderReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(n) => self.position.checked_add_signed(n),
            SeekFrom::End(n) => self.size.checked_add_signed(n),
        }
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        if position > self.size {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        self.position = position;
        Ok(position)
    }
}

// Read only the encoded-header stream description as a synthetic plain
// header. This lets us bound decoded metadata, dictionary allocation and KDF
// work before the library decompresses/decrypts the real metadata stream.
fn validate_encoded_header(header: &[u8]) -> Result<()> {
    if header.first() != Some(&0x17) {
        return Ok(());
    }
    let mut plain = vec![1, 4];
    plain.extend_from_slice(&header[1..]);
    plain.push(0);
    let mut archive_bytes = b"7z\xbc\xaf\x27\x1c\x00\x04".to_vec();
    archive_bytes.extend([0; 4]);
    archive_bytes.extend(0u64.to_le_bytes());
    archive_bytes.extend((plain.len() as u64).to_le_bytes());
    archive_bytes.extend(pesto::yenc::crc32(&plain).to_le_bytes());
    let crc = pesto::yenc::crc32(&archive_bytes[12..32]);
    archive_bytes[8..12].copy_from_slice(&crc.to_le_bytes());
    archive_bytes.extend(plain);
    let description = Archive::read(&mut io::Cursor::new(archive_bytes), &Password::empty())
        .context("invalid 7z encoded header description")?;
    ensure!(
        description.blocks.len() == 1,
        "unsupported 7z metadata stream layout"
    );
    ensure!(
        description
            .pack_sizes()
            .iter()
            .all(|s| *s <= 2 * 1024 * 1024),
        "7z packed metadata exceeds inspection limit"
    );
    for block in &description.blocks {
        ensure!(
            block.get_unpack_size() <= 2 * 1024 * 1024,
            "7z decoded metadata exceeds inspection limit"
        );
        for coder in &block.coders {
            let properties = coder.properties();
            match coder.encoder_method_id() {
                [6, 0xf1, 7, 1] => {
                    let power = properties
                        .first()
                        .context("missing 7z header encryption properties")?
                        & 0x3f;
                    ensure!(
                        power <= 20 || power == 0x3f,
                        "7z header password derivation exceeds inspection CPU limit"
                    );
                }
                [3, 1, 1] => {
                    ensure!(properties.len() >= 5, "truncated 7z header LZMA properties");
                    ensure!(
                        u32::from_le_bytes(properties[1..5].try_into()?) <= 64 * 1024 * 1024,
                        "7z metadata dictionary exceeds inspection memory limit"
                    );
                }
                [0x21] => {
                    ensure!(
                        properties.first().is_some_and(|p| *p <= 28),
                        "7z metadata dictionary exceeds inspection memory limit"
                    );
                }
                [0] => (),
                _ => anyhow::bail!("unsupported compressed 7z metadata method"),
            }
        }
    }
    Ok(())
}

pub(super) async fn media<S: Source>(
    remote: &mut Remote<S>,
    file: usize,
    password: Option<&str>,
) -> Result<Media> {
    let size = remote.size(file).await?;
    let start = remote.read(file, 0, 32).await?;
    ensure!(
        start.starts_with(b"7z\xbc\xaf\x27\x1c"),
        "invalid 7z signature"
    );
    ensure!(
        pesto::yenc::crc32(&start[12..]) == u32::from_le_bytes(start[8..12].try_into()?),
        "7z start header CRC mismatch"
    );
    let header_size = u64::from_le_bytes(start[20..28].try_into()?);
    ensure!(
        header_size <= 2 * 1024 * 1024,
        "7z header exceeds partial inspection limit"
    );
    let header_offset = 32u64
        .checked_add(u64::from_le_bytes(start[12..20].try_into()?))
        .context("7z header offset overflow")?;
    ensure!(
        header_offset <= size && header_size <= size - header_offset,
        "7z header exceeds archive size"
    );
    let header = remote
        .read(file, header_offset, header_size as usize)
        .await?;
    ensure!(
        pesto::yenc::crc32(&header) == u32::from_le_bytes(start[28..32].try_into()?),
        "7z next header CRC mismatch"
    );
    validate_encoded_header(&header)?;
    let mut reader = HeaderReader {
        size,
        position: 0,
        chunks: BTreeMap::from([(0, start), (header_offset, header)]),
        missing: None,
    };
    let archive = loop {
        let key = Password::from(password.unwrap_or(""));
        let (result, mut returned) = tokio::task::spawn_blocking(move || {
            reader.position = 0;
            reader.missing = None;
            let result = Archive::read(&mut reader, &key);
            (result, reader)
        })
        .await
        .context("7z metadata parser task failed")?;
        if let Some((offset, length)) = returned.missing {
            let cached: usize = returned.chunks.values().map(Vec::len).sum();
            ensure!(
                cached + length <= 4 * 1024 * 1024,
                "7z metadata exceeds partial inspection limit"
            );
            returned
                .chunks
                .insert(offset, remote.read(file, offset, length).await?);
            reader = returned;
        } else {
            break match result {
                Err(sevenz_rust2::Error::PasswordRequired) => anyhow::bail!(
                    "archive requires a password; use --password or the NZB password metadata"
                ),
                other => {
                    other.context("opening 7z metadata (incorrect password or damaged archive)")?
                }
            };
        }
    };
    let (entry_index, entry) = archive
        .files
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            e.has_stream && !e.is_directory && !e.is_anti_item && is_media_name(&e.name)
        })
        .max_by_key(|(_, e)| (!e.name.to_ascii_lowercase().contains("sample"), e.size))
        .context("7z archive does not contain a supported media file")?;
    let block_index = archive
        .stream_map
        .file_block_index
        .get(entry_index)
        .copied()
        .flatten()
        .context("7z media block is missing")?;
    let block = archive
        .blocks
        .get(block_index)
        .context("7z media block is invalid")?;
    // Copy and AES are independently seekable; compression and delta/BCJ
    // filters require prior decoded state and must not be mistaken for Copy.
    for coder in &block.coders {
        ensure!(
            matches!(coder.encoder_method_id(), [0] | [6, 0xf1, 7, 1]),
            "7z media is compressed or uses filters; partial MediaInfo extraction is unavailable"
        );
    }
    let aes: Vec<_> = block
        .coders
        .iter()
        .filter(|c| c.encoder_method_id() == [6, 0xf1, 7, 1])
        .collect();
    ensure!(aes.len() <= 1, "unsupported 7z encryption chain");
    let cipher = if let Some(coder) = aes.first() {
        Some(crypto::sevenz(coder.properties(), password).await?)
    } else {
        None
    };
    let streams = archive.stream_map.block_first_pack_stream_index();
    let stream = *streams
        .get(block_index)
        .context("7z packed stream is missing")?;
    let next_stream = streams
        .get(block_index + 1)
        .copied()
        .unwrap_or(archive.pack_sizes().len());
    ensure!(
        next_stream == stream + 1,
        "7z multi-input media stream cannot be inspected partially"
    );
    let offset = 32u64
        .checked_add(archive.pack_pos())
        .and_then(|p| p.checked_add(*archive.stream_map.pack_stream_offsets().get(stream)?))
        .context("7z packed offset overflow")?;
    let packed = *archive
        .pack_sizes()
        .get(stream)
        .context("7z packed size is missing")?;
    let unpacked = block.get_unpack_size();
    ensure!(
        packed >= unpacked && packed - unpacked < if cipher.is_some() { 16 } else { 1 },
        "invalid 7z stored stream size"
    );
    ensure!(
        offset <= size && packed <= size - offset,
        "7z packed stream exceeds archive size"
    );
    let first_file = *archive
        .stream_map
        .block_first_file_index
        .get(block_index)
        .context("7z block file index is missing")?;
    let plaintext_offset = archive
        .files
        .get(first_file..entry_index)
        .context("7z media file order is invalid")?
        .iter()
        .filter(|e| e.has_stream)
        .try_fold(0u64, |n, e| n.checked_add(e.size))
        .context("7z media offset overflow")?;
    ensure!(
        plaintext_offset <= unpacked && entry.size <= unpacked - plaintext_offset,
        "7z media exceeds decoded block size"
    );
    // AES ranges start at the beginning of the encrypted block; plain Copy
    // ranges can instead start directly at this entry's logical offset.
    let (offset, plaintext_offset) = if cipher.is_some() {
        (offset, plaintext_offset)
    } else {
        (offset + plaintext_offset, 0)
    };
    Ok(Media {
        name: entry.name.clone(),
        spans: vec![Span {
            file,
            offset,
            size: entry.size,
            crypto: cipher,
            plaintext_offset,
            cbc_previous: None,
        }],
        expected_size: None,
        volumes: vec![],
        password: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoded_header_limits_are_checked_before_decompression() {
        let mut header = vec![0x17, 6, 0, 1, 9, 10, 0, 7, 11, 1, 0, 1, 1, 0, 12, 0xff];
        header.extend((3 * 1024 * 1024u64).to_le_bytes());
        header.extend([0, 0]);
        assert!(validate_encoded_header(&header)
            .unwrap_err()
            .to_string()
            .contains("decoded metadata exceeds"));
    }
    #[test]
    fn uncached_ranges_never_appear_as_zero_filled_data() {
        let mut reader = HeaderReader {
            size: 100,
            position: 0,
            chunks: BTreeMap::from([(0, vec![1, 2, 3])]),
            missing: None,
        };
        let mut out = [0; 8];
        assert_eq!(reader.read(&mut out).unwrap(), 3);
        assert_eq!(&out[..3], &[1, 2, 3]);
        assert_eq!(
            reader.read(&mut out).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(reader.missing, Some((3, 97)));
        assert_eq!(reader.seek(SeekFrom::End(-1)).unwrap(), 99);
        assert!(reader.seek(SeekFrom::End(1)).is_err());
    }
}
