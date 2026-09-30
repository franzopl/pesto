//! Format-Preserving Encryption for yEnc control lines (NIST SP 800-38G FF1 over Radix 253).

use aes::Aes256;
use anyhow::{bail, Result};
use fpe::ff1::{FlexibleNumeralString, FF1};

use super::kdf::EncryptionSession;

/// Map byte octet to numeral 0..252 per yEnc Control Lines Standard v1.0.
pub fn byte_to_numeral(b: u8) -> Result<u16> {
    match b {
        0x01..=0x09 => Ok((b - 1) as u16),
        0x0B => Ok(9),
        0x0C => Ok(10),
        0x0E..=0xFF => Ok((b - 3) as u16),
        _ => bail!(
            "byte 0x{:02x} is outside the 253-byte Alphabet (0x00, 0x0A, 0x0D forbidden)",
            b
        ),
    }
}

/// Map numeral 0..252 back to byte octet per yEnc Control Lines Standard v1.0.
pub fn numeral_to_byte(n: u16) -> Result<u8> {
    match n {
        0..=8 => Ok((n + 1) as u8),
        9 => Ok(0x0B),
        10 => Ok(0x0C),
        11..=252 => Ok((n + 3) as u8),
        _ => bail!("numeral {n} is out of range [0, 252]"),
    }
}

/// Extract and validate the 16-byte random salt from the first encrypted control line.
pub fn extract_salt_from_line1(line1: &[u8]) -> Result<[u8; 16]> {
    if line1.len() < 18 {
        bail!("LINE_TRUNCATED: line 1 length is {} < 18", line1.len());
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
    Ok(salt)
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
pub fn split_lines_preserving_endings(input: &[u8]) -> Vec<LineSlice<'_>> {
    let mut lines = Vec::new();
    let mut pos = 0;
    while pos < input.len() {
        let start = pos;
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
/// Follows NIST SP 800-38G FF1 and yEnc Control Lines Standard v1.0:
/// - Physical lineIndex is 1-based, incrementing on every line (header, data, footer).
/// - Line 1 (=ybegin) prepends 16-byte random salt to ciphertext.
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

    let mut out = Vec::with_capacity(yenc_block.len() + 16);
    let salt = session.salt();

    for (i, line) in lines.iter().enumerate() {
        let line_index = (i + 1) as u32;
        if line.content.starts_with(b"=y") {
            let tweak = session.derive_control_tweak(segment_index, line_index);
            let ct = ff1_encrypt_line(session.control_key(), &tweak, line.content)?;
            if line_index == 1 {
                out.extend_from_slice(&salt);
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
/// - Line 1: extracts 16-byte salt, decrypts, and verifies `=ybegin`.
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

    let n = lines.len();
    let mut out = Vec::with_capacity(yenc_block.len().saturating_sub(16));

    // Process line 1
    let line1 = &lines[0];
    let salt = extract_salt_from_line1(line1.content)?;
    if salt != session.salt() {
        bail!("salt mismatch: line 1 salt does not match session salt");
    }
    let ct1 = &line1.content[16..];
    let tweak1 = session.derive_control_tweak(segment_index, 1);
    let pt1 = ff1_decrypt_line(session.control_key(), &tweak1, ct1)?;
    if !pt1.starts_with(b"=ybegin") {
        bail!("CONTROL_LINE_DECRYPT_FAILURE: line 1 decrypted text does not start with =ybegin");
    }
    out.extend_from_slice(&pt1);
    out.extend_from_slice(line1.ending);

    // Process lines 2..N-1
    let mut in_header = true;
    for (i, line) in lines.iter().enumerate().take(n.saturating_sub(1)).skip(1) {
        let line_index = (i + 1) as u32;
        if in_header {
            let tweak = session.derive_control_tweak(segment_index, line_index);
            match ff1_decrypt_line(session.control_key(), &tweak, line.content) {
                Ok(pt) if pt.starts_with(b"=ypart ") => {
                    out.extend_from_slice(&pt);
                    out.extend_from_slice(line.ending);
                }
                Ok(pt) if pt.starts_with(b"=yencryption ") => {
                    out.extend_from_slice(&pt);
                    out.extend_from_slice(line.ending);
                    in_header = false;
                }
                _ => {
                    in_header = false;
                    out.extend_from_slice(line.content);
                    out.extend_from_slice(line.ending);
                }
            }
        } else {
            out.extend_from_slice(line.content);
            out.extend_from_slice(line.ending);
        }
    }

    // Process line N (footer) if N > 1
    if n > 1 {
        let footer = &lines[n - 1];
        let line_index = n as u32;
        let tweak = session.derive_control_tweak(segment_index, line_index);
        let pt_footer = ff1_decrypt_line(session.control_key(), &tweak, footer.content)?;
        if !pt_footer.starts_with(b"=yend") {
            bail!("CONTROL_LINE_DECRYPT_FAILURE: footer decrypted text does not start with =yend");
        }
        out.extend_from_slice(&pt_footer);
        out.extend_from_slice(footer.ending);
    }

    Ok(out)
}
