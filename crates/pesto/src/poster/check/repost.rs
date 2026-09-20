use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::warn;

use crate::article::{default_subject, generate_message_id, obfuscated_name, random_from, Article};
use crate::config::{Config, ObfuscateMode};
use crate::crypto::UploadEncryptionAdapter;
use crate::nntp::pool::ConnectionSlot;
use crate::yenc;

use super::PostedSegment;

/// Re-read `seg`'s slice from disk, re-encode it, and post it under a fresh
/// Message-ID. Deliberately never reuses `seg.message_id` — see the module
/// doc comment for why reposting under a cursed ID is unsafe.
///
/// `adapter` is the upload's [`UploadEncryptionAdapter`]: when present, the
/// reposted article is re-encrypted with the *same* salt and
/// `seg.segment_index` the original copy used, so the reposted wire output
/// keeps the authenticated identity of the first attempt. Posting plaintext
/// for an encrypted upload would leak the file contents on the wire and
/// produce an article the NZB's declared `yenc_encrypted` provenance cannot
/// decrypt (Body Encryption Standard v1.2 §8).
pub(super) async fn repost_one(
    config: &Config,
    adapter: Option<&UploadEncryptionAdapter>,
    slot: &mut ConnectionSlot,
    seg: &PostedSegment,
    groups: &[String],
) -> anyhow::Result<PostedSegment> {
    let offset = (seg.part as u64 - 1) * config.article_size as u64;

    let mut file = tokio::fs::File::open(&seg.file_path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let read_len = (seg.file_size - offset).min(config.article_size as u64) as usize;
    let mut buf = Vec::new();
    buf.try_reserve_exact(read_len)
        .map_err(|e| anyhow::anyhow!("allocating repost buffer: {e}"))?;
    buf.resize(read_len, 0);
    file.read_exact(&mut buf).await?;

    // PESTO-FINAL-1 (Body Encryption Standard v1.2 §8): the original copy
    // was encrypted under this segment's salt + `seg.segment_index`, which
    // already authenticated its plaintext. If the source changed since that
    // post, re-encrypting the re-read bytes under the same identity would
    // publish a second, different plaintext under one authenticated
    // identity — refuse (Err), so the caller's existing repost-failure
    // handling exhausts attempts and leaves the segment missing. Describe
    // the condition only: no digests, keys or plaintext bytes are logged.
    if let Some(expected) = seg.plaintext_digest {
        use sha2::{Digest, Sha256};
        if Sha256::digest(&buf).as_slice() != expected.as_slice() {
            return Err(anyhow::anyhow!(
                "repost: source file changed since the original encrypted post; \
                 refusing to re-post segment {}/{} under the same segment identity",
                seg.part,
                seg.total
            ));
        }
    }

    let spec = yenc::PartSpec {
        number: seg.part,
        total: seg.total,
        offset,
    };
    let file_crc32 = (seg.part == seg.total).then_some(seg.full_crc32);
    let (wire_subject, wire_yenc, from, date) = match config.obfuscate {
        ObfuscateMode::Full => (
            obfuscated_name(),
            seg.wire_yenc_name.to_string(),
            random_from(),
            seg.date.clone(),
        ),
        ObfuscateMode::Article => (
            obfuscated_name(),
            if adapter.is_some() {
                // Encrypted upload (Body Encryption Standard v1.2 §8): the
                // yEnc `name=` is an AAD input (§2.1), so the repost must
                // reuse the original copy's name — with the same salt and
                // `seg.segment_index` below, the re-encrypted wire form stays
                // byte-identical to the first attempt. A fresh name here
                // would change the ciphertext of an article the NZB's
                // bootstrap already authenticates under the old name.
                seg.wire_yenc_name.to_string()
            } else {
                // Ordinary upload: the name is not authenticated, keep the
                // upstream per-repost rotation.
                obfuscated_name()
            },
            random_from(),
            super::super::resolve_date(config.date.as_deref()),
        ),
        _ => (
            seg.wire_name.to_string(),
            seg.wire_yenc_name.to_string(),
            seg.from.to_string(),
            seg.date.clone(),
        ),
    };
    // `seg.subject_name` is always the *real* filename (see `PostedSegment`'s
    // doc comment) — using it here would repost an obfuscated release under
    // its real name, undoing `--obfuscate` the moment one article needs a
    // repost. `wire_name` carries the identity actually posted with.
    let encoded = match adapter {
        Some(adapter) => {
            let mut encode_buf = Vec::new();
            adapter
                .encode_article(
                    &wire_yenc,
                    seg.file_size,
                    spec,
                    &buf,
                    config.line_length,
                    seg.segment_index,
                    &mut encode_buf,
                )
                .map_err(|e| anyhow::anyhow!("repost: failed to re-encrypt article segment: {e}"))?
        }
        None => yenc::encode_part(
            &wire_yenc,
            seg.file_size,
            spec,
            &buf,
            config.line_length,
            file_crc32,
        ),
    };
    let (rfc_date, _ts) = &date;
    let mut message_id = generate_message_id(config.message_id_domain.as_deref());
    let article = Article {
        message_id: message_id.clone(),
        from: from.clone(),
        newsgroups: groups.to_vec(),
        subject: default_subject(
            &wire_subject,
            seg.part,
            seg.total,
            (seg.total_files > 0).then_some((seg.file_index, seg.total_files)),
        ),
        date: rfc_date.clone(),
        no_archive: config.no_archive,
    };
    let headers = article.build_headers();
    let wire_bytes = (headers.len() + encoded.body.len()) as u64;
    // NZB `<segment bytes>` semantics (Body Encryption Standard v1.2
    // §2.1): encrypted releases declare the plaintext
    // segment length, ordinary uploads the wire-article length.
    let nzb_bytes = if adapter.is_some() {
        buf.len() as u64
    } else {
        wire_bytes
    };

    let max_retries = config.retries.max(1);
    let mut last_err = anyhow::anyhow!("repost: no attempt made");
    for attempt in 1..=max_retries {
        match slot.ensure_connected().await {
            Ok(conn) => match conn.repost_parts_confirmed(&headers, &encoded.body).await {
                Ok(returned_id) => {
                    if let Some(server_id) = returned_id {
                        if server_id != message_id {
                            warn!(
                                sent = %message_id,
                                returned = %server_id,
                                "server returned a different Message-ID than sent; adopting it"
                            );
                            message_id = server_id;
                        }
                    }
                    return Ok(PostedSegment {
                        file_name: seg.file_name.clone(),
                        file_path: seg.file_path.clone(),
                        subject_name: seg.subject_name.clone(),
                        wire_name: Arc::from(wire_subject.as_str()),
                        wire_yenc_name: Arc::from(wire_yenc.as_str()),
                        file_size: seg.file_size,
                        part: seg.part,
                        total: seg.total,
                        message_id,
                        bytes: nzb_bytes,
                        from: Arc::from(from.as_str()),
                        date: date.clone(),
                        full_crc32: seg.full_crc32,
                        server_idx: slot.server_idx(),
                        file_index: seg.file_index,
                        total_files: seg.total_files,
                        segment_index: seg.segment_index,
                        plaintext_digest: seg.plaintext_digest,
                    });
                }
                Err(e) => {
                    slot.invalidate("post_err");
                    last_err = e;
                }
            },
            Err(e) => {
                last_err = e;
            }
        }
        if attempt < max_retries {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    Err(last_err)
}

/// NNTP response code embedded in a POST error, if any.
fn nntp_error_code(err: &anyhow::Error) -> Option<u16> {
    let s = err.to_string();
    for prefix in [
        "article rejected by server (",
        "authentication rejected by server (code ",
        "unexpected POST response: ",
        "unexpected POST response (pipelined): ",
        "POST not permitted: ",
        "POST not permitted (pipelined): ",
    ] {
        if let Some(rest) = s.split_once(prefix).map(|(_, r)| r) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(code) = digits.parse::<u16>() {
                if (200..600).contains(&code) {
                    return Some(code);
                }
            }
        }
    }
    None
}

/// True when a repost failed because the server refused the article (441 or
/// other 4xx except the AUTH 48x class). Connect/timeout/5xx/AUTH 480–489
/// are Inconclusive.
pub(super) fn is_post_refusal(err: &anyhow::Error) -> bool {
    let s = err.to_string();
    if s.contains("authentication rejected by server") {
        return false;
    }
    match nntp_error_code(err) {
        Some(480..=489) => false,
        Some(code) if (400..500).contains(&code) => true,
        _ => false,
    }
}
