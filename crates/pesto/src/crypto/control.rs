//! Format-Preserving Encryption for yEnc control lines (NIST SP 800-38G FF1 over Radix 253).

use aes::Aes256;
use anyhow::{bail, Context, Result};
use fpe::ff1::{FlexibleNumeralString, FF1};

use super::kdf::EncryptionSession;
use super::ybegin::name_token_boundary;

/// Map byte octet to numeral 0..252 per yEnc Control Lines Standard v1.2.
pub fn byte_to_numeral(b: u8) -> Result<u16> {
    match b {
        0x01..=0x09 => Ok((b - 1) as u16),
        0x0B => Ok(9),
        0x0C => Ok(10),
        0x0E..=0xFF => Ok((b - 3) as u16),
        _ => bail!("byte 0x{b:02x} is outside the 253-byte Alphabet (0x00, 0x0A, 0x0D forbidden)"),
    }
}

/// Map numeral 0..252 back to byte octet per yEnc Control Lines Standard v1.2.
pub fn numeral_to_byte(n: u16) -> Result<u8> {
    match n {
        0..=8 => Ok((n + 1) as u8),
        9 => Ok(0x0B),
        10 => Ok(0x0C),
        11..=252 => Ok((n + 3) as u8),
        _ => bail!("numeral {n} is out of range [0, 252]"),
    }
}

pub const BOOTSTRAP_PREFIX_LEN: usize = 20;

/// Extract and validate the 20-byte bootstrap prefix ([16B salt][4B uint32_be(segmentIndex)])
/// from the first encrypted control line.
pub fn extract_bootstrap_from_line1(line1: &[u8]) -> Result<([u8; 16], u32)> {
    if line1.len() < 22 {
        bail!("LINE_TRUNCATED: line 1 length is {} < 22", line1.len());
    }
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&line1[0..16]);
    for &b in &salt {
        if b == 0x00 || b == 0x0A || b == 0x0D {
            bail!(
                "INVALID_SALT_CHARACTER: salt contains forbidden byte 0x{:02x}",
                b
            );
        }
    }
    let segment_index = u32::from_be_bytes(line1[16..20].try_into().unwrap());
    if segment_index == 0 {
        bail!("ZERO_SEGMENT_INDEX: segment index cannot be zero");
    }
    // Index framing rule (Control Std v1.2 §5/§8): a segmentIndex whose big-endian encoding
    // contains 0x0A (LF) or 0x0D (CR) would have split Line 1 on the wire —
    // reject (wire corruption; retriable on an alternate provider).
    if segment_index
        .to_be_bytes()
        .iter()
        .any(|&b| b == 0x0A || b == 0x0D)
    {
        bail!("FORBIDDEN_SEGMENT_INDEX_BYTE: segment index bytes contain 0x0A or 0x0D");
    }
    Ok((salt, segment_index))
}

/// Generate a 16-byte random session salt sampled strictly from the 253-byte
/// Alphabet (excluding forbidden bytes 0x00, 0x0A, and 0x0D) via rejection sampling.
pub fn generate_alphabet_salt() -> [u8; 16] {
    use rand::Rng;
    let mut rng = rand::rng();
    let mut salt = [0u8; 16];
    for b in &mut salt {
        loop {
            let candidate: u8 = rng.random();
            if candidate != 0x00 && candidate != 0x0A && candidate != 0x0D {
                *b = candidate;
                break;
            }
        }
    }
    salt
}

/// Encrypt a single control line's content using FF1 over Radix 253.
pub fn ff1_encrypt_line(key: &[u8; 32], tweak: &[u8; 8], plaintext: &[u8]) -> Result<Vec<u8>> {
    if plaintext.len() < 2 {
        bail!(
            "LINE_TOO_SHORT: control line length {} < 2",
            plaintext.len()
        );
    }
    let ff1 =
        FF1::<Aes256>::new(key, 253).map_err(|e| anyhow::anyhow!("ff1 init failed: {e:?}"))?;
    let numerals: Result<Vec<u16>> = plaintext.iter().map(|&b| byte_to_numeral(b)).collect();
    let numerals = numerals?;
    let num_str = FlexibleNumeralString::from(numerals);
    let encrypted = ff1
        .encrypt(tweak, &num_str)
        .map_err(|e| anyhow::anyhow!("ff1 encrypt failed: {e:?}"))?;
    let ct_numerals: Vec<u16> = encrypted.into();
    ct_numerals.into_iter().map(numeral_to_byte).collect()
}

/// Decrypt a single control line's content using FF1 over Radix 253.
pub fn ff1_decrypt_line(key: &[u8; 32], tweak: &[u8; 8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if ciphertext.len() < 2 {
        bail!(
            "LINE_TOO_SHORT: control line length {} < 2",
            ciphertext.len()
        );
    }
    let ff1 =
        FF1::<Aes256>::new(key, 253).map_err(|e| anyhow::anyhow!("ff1 init failed: {e:?}"))?;
    let numerals: Result<Vec<u16>> = ciphertext.iter().map(|&b| byte_to_numeral(b)).collect();
    let numerals = numerals?;
    let num_str = FlexibleNumeralString::from(numerals);
    let decrypted = ff1
        .decrypt(tweak, &num_str)
        .map_err(|e| anyhow::anyhow!("ff1 decrypt failed: {e:?}"))?;
    let pt_numerals: Vec<u16> = decrypted.into();
    pt_numerals.into_iter().map(numeral_to_byte).collect()
}

#[derive(Debug, Clone, Copy)]
pub struct LineSlice<'a> {
    pub content: &'a [u8],
    pub ending: &'a [u8],
}

/// Split a buffer into lines while preserving line terminators (`\r\n` or `\n`).
///
/// For encrypted wire articles (which do not begin with `=y`), Line 1 carries a
/// 20-byte bootstrap prefix ([16B salt][4B uint32_be(segmentIndex)]). Because
/// uint32_be(segmentIndex) may contain 0x0A (LF) or 0x0D (CR), the Line 1 terminator
/// is searched strictly after the 20-byte bootstrap prefix.
pub fn split_lines_preserving_endings(input: &[u8]) -> Vec<LineSlice<'_>> {
    let mut lines = Vec::new();
    let mut pos = 0;
    while pos < input.len() {
        let start = pos;
        if lines.is_empty() && !input.starts_with(b"=y") {
            if input.len() >= BOOTSTRAP_PREFIX_LEN {
                pos += BOOTSTRAP_PREFIX_LEN;
            } else {
                // Encrypted article too short to carry the 20-byte bootstrap
                // prefix: treat the entire input as one truncated line 1 so a
                // stray 0x0A inside the (missing) prefix cannot fragment it.
                // `extract_bootstrap_from_line1` then reports LINE_TRUNCATED.
                pos = input.len();
            }
        }
        while pos < input.len() && input[pos] != b'\n' {
            pos += 1;
        }
        if pos < input.len() {
            // input[pos] == b'\n'
            pos += 1;
            let line_slice = &input[start..pos];
            if line_slice.ends_with(b"\r\n") {
                lines.push(LineSlice {
                    content: &line_slice[..line_slice.len() - 2],
                    ending: b"\r\n",
                });
            } else if line_slice.ends_with(b"\n") {
                lines.push(LineSlice {
                    content: &line_slice[..line_slice.len() - 1],
                    ending: b"\n",
                });
            } else {
                lines.push(LineSlice {
                    content: line_slice,
                    ending: b"",
                });
            }
        } else {
            lines.push(LineSlice {
                content: &input[start..pos],
                ending: b"",
            });
        }
    }
    lines
}

/// Encrypt control lines in a yEnc block, preserving data lines and line terminators.
///
/// Follows NIST SP 800-38G FF1 and yEnc Control Lines Standard v1.2:
/// - Physical lineIndex is 1-based, incrementing on every line (header, data, footer).
/// - Line 1 (=ybegin) prepends 20-byte bootstrap prefix ([16B salt][4B uint32_be(segmentIndex)]) to ciphertext.
/// - Lines 2..N preserve exact length.
/// - Data lines (not starting with `=y`) remain untouched.
pub fn encrypt_yenc_control_lines(
    session: &EncryptionSession,
    segment_index: u32,
    yenc_block: &[u8],
) -> Result<Vec<u8>> {
    let lines = split_lines_preserving_endings(yenc_block);
    if lines.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(yenc_block.len() + BOOTSTRAP_PREFIX_LEN);
    let salt = session.salt();

    for (i, line) in lines.iter().enumerate() {
        let line_index = (i + 1) as u32;
        if line.content.starts_with(b"=y") {
            let tweak = session.derive_control_tweak(segment_index, line_index);
            let ct = ff1_encrypt_line(session.control_key(), &tweak, line.content)?;
            if line_index == 1 {
                out.extend_from_slice(&salt);
                out.extend_from_slice(&segment_index.to_be_bytes());
            }
            out.extend_from_slice(&ct);
            out.extend_from_slice(line.ending);
        } else {
            out.extend_from_slice(line.content);
            out.extend_from_slice(line.ending);
        }
    }

    Ok(out)
}

/// Decrypt control lines in a yEnc block, restoring original control lines.
///
/// - Line 1: extracts 20-byte bootstrap prefix ([16B salt][4B uint32_be(segmentIndex)]), decrypts, and verifies `=ybegin`.
/// - Subsequent header lines: decrypts until a data line is encountered.
/// - Data lines: untouched.
/// - Footer line: decrypts with lineIndex=N, verifies `=yend`.
pub fn decrypt_yenc_control_lines(
    session: &EncryptionSession,
    segment_index: u32,
    yenc_block: &[u8],
) -> Result<Vec<u8>> {
    let lines = split_lines_preserving_endings(yenc_block);
    if lines.is_empty() {
        return Ok(Vec::new());
    }

    let mut trailing_count = 0;
    while trailing_count < lines.len() && lines[lines.len() - 1 - trailing_count].content.is_empty()
    {
        trailing_count += 1;
    }
    let (active_lines, trailing_blanks) = lines.split_at(lines.len() - trailing_count);
    if active_lines.is_empty() {
        return Ok(Vec::new());
    }

    let n = active_lines.len();
    let mut out = Vec::with_capacity(yenc_block.len().saturating_sub(BOOTSTRAP_PREFIX_LEN));

    // Process line 1. The bootstrap is re-validated here (salt == session
    // salt, index == the passed index) because this function is also called
    // directly by tests/callers that did not pre-extract the bootstrap — the
    // checks are the §5 wire-corruption gate for any caller-provided index.
    let line1 = &active_lines[0];
    let (salt, line1_segment_index) = extract_bootstrap_from_line1(line1.content)?;
    if salt != session.salt() {
        bail!("salt mismatch: line 1 salt does not match session salt");
    }
    if line1_segment_index != segment_index {
        bail!("DUAL_INDEX_MISMATCH: line 1 segment index {line1_segment_index} does not match expected {segment_index}");
    }
    let ct1 = &line1.content[BOOTSTRAP_PREFIX_LEN..];
    let tweak1 = session.derive_control_tweak(segment_index, 1);
    let pt1 = ff1_decrypt_line(session.control_key(), &tweak1, ct1)?;
    if !pt1.starts_with(b"=ybegin") {
        // Wrong-password signature: FF1 is a permutation, so a Line 1
        // encrypted under a different password still decrypts "successfully"
        // — to pseudorandom garbage that cannot be `=ybegin` (wire
        // corruption; retriable on an alternate provider).
        bail!("CONTROL_LINE_DECRYPT_FAILURE: line 1 decrypted text does not start with =ybegin");
    }
    out.extend_from_slice(&pt1);
    out.extend_from_slice(line1.ending);

    // Process lines 2..N-1
    //
    // Deterministic combined-mode restoration (Control Std v1.2 §5, step 4):
    // Physical line 2 is decrypted first using lineIndex = 2.
    // - If line 2 starts with `=ypart `, it is multipart: line 3 must decrypt to `=yencryption `,
    //   and restored `=ybegin` (pt1) must contain `part=` (otherwise INVALID_MULTIPART_FRAMING).
    // - If line 2 starts with `=yencryption `, it is single-part:
    //   and restored `=ybegin` (pt1) must NOT contain `part=` (otherwise MISPLACED_ENCRYPTION_HEADER).
    // - If line 2 starts with neither, fail with CONTROL_LINE_DECRYPT_FAILURE.
    let pt1_has_part = name_token_boundary(&pt1)
        .map(|name_idx| {
            let kv_prefix = &pt1[..name_idx];
            kv_prefix
                .split(|&b| b == b' ' || b == b'\t')
                .any(|tok| tok.starts_with(b"part="))
        })
        .unwrap_or(false);

    if n < 3 {
        bail!("LINE_TRUNCATED: encrypted article has fewer than 3 lines");
    }

    // Line 2: Decrypt with lineIndex=2 per Control Std v1.2 §5, step 4.
    let line2 = &active_lines[1];
    let tweak2 = session.derive_control_tweak(segment_index, 2);
    let pt2 = ff1_decrypt_line(session.control_key(), &tweak2, line2.content)
        .context("control-line decryption failed at line 2 in the header region")?;

    let is_multipart = if pt2.starts_with(b"=ypart ") {
        true
    } else if pt2.starts_with(b"=yencryption ") {
        false
    } else {
        bail!("CONTROL_LINE_DECRYPT_FAILURE: line 2 decrypted text starts with neither =ypart nor =yencryption");
    };

    let d_start = if is_multipart {
        if n < 4 {
            bail!("LINE_TRUNCATED: multipart encrypted article has fewer than 4 lines");
        }
        if !pt1_has_part {
            bail!("INVALID_MULTIPART_FRAMING: multipart article missing part= on line 1");
        }

        out.extend_from_slice(&pt2);
        out.extend_from_slice(line2.ending);

        // Line 3: =yencryption
        let line3 = &active_lines[2];
        let tweak3 = session.derive_control_tweak(segment_index, 3);
        let pt3 = ff1_decrypt_line(session.control_key(), &tweak3, line3.content)
            .context("control-line decryption failed at line 3 in the header region")?;
        if !pt3.starts_with(b"=yencryption ") {
            bail!("CONTROL_LINE_DECRYPT_FAILURE: line 3 decrypted text does not start with =yencryption");
        }
        out.extend_from_slice(&pt3);
        out.extend_from_slice(line3.ending);

        3
    } else {
        if pt1_has_part {
            bail!("MISPLACED_ENCRYPTION_HEADER: single-part article has part= parameter on line 1");
        }

        out.extend_from_slice(&pt2);
        out.extend_from_slice(line2.ending);

        2
    };

    // Data lines: d_start..n-1 pass through untouched
    for line in &active_lines[d_start..n - 1] {
        out.extend_from_slice(line.content);
        out.extend_from_slice(line.ending);
    }

    // Process line N (footer)
    let footer = &active_lines[n - 1];
    let line_index = n as u32;
    let tweak = session.derive_control_tweak(segment_index, line_index);
    let pt_footer = ff1_decrypt_line(session.control_key(), &tweak, footer.content)?;
    if !pt_footer.starts_with(b"=yend") {
        bail!("CONTROL_LINE_DECRYPT_FAILURE: footer decrypted text does not start with =yend");
    }
    out.extend_from_slice(&pt_footer);
    out.extend_from_slice(footer.ending);

    for blank in trailing_blanks {
        out.extend_from_slice(blank.content);
        out.extend_from_slice(blank.ending);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_kdf_input() -> &'static str {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test-vectors/body_encryption.json");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let fixture: serde_json::Value =
            serde_json::from_str(&content).expect("body_encryption.json must parse");
        static VALUE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        VALUE
            .get_or_init(|| {
                fixture["vectors"][0]["password"]
                    .as_str()
                    .expect("body_encryption.json vectors[0].password")
                    .to_owned()
            })
            .as_str()
    }

    #[test]
    fn test_decrypt_with_trailing_blank_lines() {
        let salt = *b"K7mX9pL2qR8vN4wZ";
        let session = EncryptionSession::new(fixture_kdf_input(), salt).unwrap();
        let segment_index = 1;
        let block = b"=ybegin line=128 size=4 name=test.bin\r\n=yencryption cipher=XChaCha20-Poly1305 salt=4b376d5839704c32715238764e34775a index=00000001 tag=0cd77ce245a654463f90b945b1d22d5b\r\ndata\r\n=yend size=4 pcrc32=12345678\r\n";
        let encrypted = encrypt_yenc_control_lines(&session, segment_index, block).unwrap();
        let mut with_trailing = encrypted.clone();
        with_trailing.extend_from_slice(b"\r\n\r\n");
        let decrypted =
            decrypt_yenc_control_lines(&session, segment_index, &with_trailing).unwrap();
        assert!(decrypted.starts_with(b"=ybegin"));
        assert!(decrypted.ends_with(b"\r\n\r\n"));
    }
}
