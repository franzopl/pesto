//! Cryptographic adapter presenting standard yEnc encode and decode article contracts.

use anyhow::{bail, ensure, Context, Result};
use std::collections::HashMap;
use std::sync::Arc;

use super::body;
use super::control;
use super::kdf::EncryptionSession;
use crate::poster::SegmentIdentity;
use crate::yenc::{self, DecodedPart, EncodedPart, PartSpec};

/// Thin adapter presenting standard yEnc encode contract for encrypted articles.
pub struct UploadEncryptionAdapter {
    session: Arc<EncryptionSession>,
}

impl UploadEncryptionAdapter {
    pub fn new(session: Arc<EncryptionSession>) -> Self {
        Self { session }
    }

    pub fn session(&self) -> &Arc<EncryptionSession> {
        &self.session
    }

    /// Encrypt and yEnc-encode a raw article segment.
    ///
    /// 1. Derives 24-byte body nonce from `identity.segment_index`.
    /// 2. Encrypts `data` via XChaCha20-Poly1305 -> `(ciphertext, tag)`.
    /// 3. yEnc-encodes `ciphertext` into `body` buffer.
    /// 4. Injects `=yencryption` control line.
    /// 5. FF1-encrypts all control lines over Radix 253, prepending 16-byte salt to line 1.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_article(
        &self,
        name: &str,
        file_size: u64,
        spec: PartSpec,
        data: &[u8],
        line_len: usize,
        file_crc32: Option<u32>,
        identity: SegmentIdentity,
        body: &mut Vec<u8>,
    ) -> Result<EncodedPart> {
        let nonce = self.session.derive_body_nonce(identity.segment_index);
        let (ciphertext, tag) = body::encrypt_body(data, self.session.master_key(), &nonce)?;

        let mut encoded = yenc::encode_part_into(
            name,
            file_size,
            spec,
            &ciphertext,
            line_len,
            file_crc32,
            body,
        );

        // Format =yencryption control line
        let mut hex_salt = String::with_capacity(32);
        for b in &self.session.salt() {
            use std::fmt::Write;
            write!(&mut hex_salt, "{:02x}", b).unwrap();
        }
        let mut hex_tag = String::with_capacity(32);
        for b in &tag {
            use std::fmt::Write;
            write!(&mut hex_tag, "{:02x}", b).unwrap();
        }
        let yenc_line = format!(
            "=yencryption cipher=XChaCha20-Poly1305 salt={} tag={}\r\n",
            hex_salt, hex_tag
        );

        // Insert =yencryption after =ypart (if multipart) or after =ybegin (if single-part)
        let insertion_idx = if spec.total > 1 {
            let ypart_rel = encoded
                .body
                .windows(7)
                .position(|w| w == b"\n=ypart")
                .context("missing =ypart line")?;
            let ypart_pos = ypart_rel + 1;
            let nl_pos = encoded.body[ypart_pos..]
                .iter()
                .position(|&b| b == b'\n')
                .context("missing newline after =ypart")?;
            ypart_pos + nl_pos + 1
        } else {
            let nl_pos = encoded
                .body
                .iter()
                .position(|&b| b == b'\n')
                .context("missing newline after =ybegin")?;
            nl_pos + 1
        };

        let mut with_yenc = Vec::with_capacity(encoded.body.len() + yenc_line.len());
        with_yenc.extend_from_slice(&encoded.body[..insertion_idx]);
        with_yenc.extend_from_slice(yenc_line.as_bytes());
        with_yenc.extend_from_slice(&encoded.body[insertion_idx..]);

        // Encrypt control lines using FF1
        let wire_body =
            control::encrypt_yenc_control_lines(&self.session, identity.segment_index, &with_yenc)?;

        encoded.body = wire_body;
        Ok(encoded)
    }
}

/// Parameters extracted from the `=yencryption` control line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YEncryptionParams {
    pub cipher: String,
    pub salt: [u8; 16],
    pub tag: [u8; 16],
}

/// Parse parameters from `=yencryption ...` control line.
///
/// Accepts only the canonical token sequence:
/// `=yencryption cipher=XChaCha20-Poly1305 salt=<32_hex_chars> tag=<32_hex_chars>`
/// Salt and tag must be exactly 32 lowercase hexadecimal characters.
/// No extra, duplicate, missing, or reordered fields are permitted.
pub fn parse_yencryption_line(line: &[u8]) -> Result<YEncryptionParams> {
    ensure!(line.starts_with(b"=yencryption"), "not a =yencryption line");
    let text =
        std::str::from_utf8(line).map_err(|_| anyhow::anyhow!("invalid utf8 in =yencryption"))?;
    let tokens: Vec<&str> = text.split_whitespace().collect();

    ensure!(
        tokens.first() == Some(&"=yencryption"),
        "line does not begin with =yencryption token"
    );

    // Reject duplicate parameters
    let mut seen_keys = std::collections::HashSet::new();
    for token in tokens.iter().skip(1) {
        if let Some((k, _)) = token.split_once('=') {
            if !seen_keys.insert(k) {
                bail!("DUPLICATE_PARAMETER: duplicate parameter in =yencryption header");
            }
        }
    }

    if tokens.len() < 2 {
        bail!("MISSING_CIPHER: cipher parameter missing");
    }

    // Token 1 must be cipher=
    if !tokens[1].starts_with("cipher=") {
        if tokens.iter().skip(1).any(|t| t.starts_with("cipher=")) {
            bail!("REORDERED_HEADER: cipher parameter out of order");
        } else {
            bail!("MISSING_CIPHER: cipher parameter missing");
        }
    }
    let cipher_val = &tokens[1]["cipher=".len()..];
    if cipher_val.is_empty() {
        bail!("INVALID_CIPHER: empty cipher parameter");
    }
    if cipher_val != "XChaCha20-Poly1305" {
        bail!("UNSUPPORTED_CIPHER: unsupported cipher {cipher_val}");
    }

    // Token 2 must be salt=
    if tokens.len() < 3 {
        bail!("MISSING_SALT: salt parameter missing");
    }
    if !tokens[2].starts_with("salt=") {
        if tokens.iter().skip(3).any(|t| t.starts_with("salt=")) {
            bail!("REORDERED_HEADER: salt parameter out of order");
        } else {
            bail!("MISSING_SALT: salt parameter missing");
        }
    }
    let salt_str = &tokens[2]["salt=".len()..];
    if salt_str.len() != 32 {
        bail!(
            "INVALID_SALT_LENGTH: salt must be exactly 32 hex characters, got {}",
            salt_str.len()
        );
    }
    if salt_str
        .chars()
        .any(|c| !c.is_ascii_digit() && !matches!(c, 'a'..='f'))
    {
        bail!("INVALID_SALT_HEX: salt contains non-hex or non-lowercase characters");
    }
    let mut salt = [0u8; 16];
    for i in 0..16 {
        salt[i] = u8::from_str_radix(&salt_str[i * 2..i * 2 + 2], 16)
            .map_err(|_| anyhow::anyhow!("INVALID_SALT_HEX: salt contains non-hex characters"))?;
    }

    // Token 3 must be tag=
    if tokens.len() < 4 {
        bail!("MISSING_TAG: tag parameter missing");
    }
    if !tokens[3].starts_with("tag=") {
        if tokens.iter().skip(1).any(|t| t.starts_with("tag=")) {
            bail!("REORDERED_HEADER: tag parameter out of order");
        } else {
            bail!("MISSING_TAG: tag parameter missing");
        }
    }
    let tag_str = &tokens[3]["tag=".len()..];
    if tag_str.len() != 32 {
        bail!(
            "INVALID_TAG_LENGTH: tag must be exactly 32 hex characters, got {}",
            tag_str.len()
        );
    }
    if tag_str
        .chars()
        .any(|c| !c.is_ascii_digit() && !matches!(c, 'a'..='f'))
    {
        bail!("INVALID_TAG_HEX: tag contains non-hex or non-lowercase characters");
    }
    let mut tag = [0u8; 16];
    for i in 0..16 {
        tag[i] = u8::from_str_radix(&tag_str[i * 2..i * 2 + 2], 16)
            .map_err(|_| anyhow::anyhow!("INVALID_TAG_HEX: tag contains non-hex characters"))?;
    }

    if tokens.len() > 4 {
        bail!("EXTRA_PARAMETER: unexpected additional parameters in =yencryption header");
    }

    Ok(YEncryptionParams {
        cipher: "XChaCha20-Poly1305".to_string(),
        salt,
        tag,
    })
}

/// Extract `=yencryption` parameters and remove the single `=yencryption` line from restored yEnc text.
///
/// Enforces strict header structure:
/// - Restored line 1 must be `=ybegin`
/// - For multipart input (line 2 is `=ypart`), line 3 must be the sole `=yencryption` line
/// - For single-part input, line 2 must be the sole `=yencryption` line
/// - Any duplicate or misplaced `=yencryption` line anywhere in the article is rejected
pub fn extract_and_remove_yencryption(input: &[u8]) -> Result<(YEncryptionParams, Vec<u8>)> {
    let lines = control::split_lines_preserving_endings(input);
    ensure!(
        lines.len() >= 2,
        "article has too few lines for encrypted yEnc headers"
    );

    ensure!(
        lines[0].content.starts_with(b"=ybegin"),
        "restored line 1 must be =ybegin"
    );

    let yenc_line_idx = if lines[1].content.starts_with(b"=ypart") {
        ensure!(
            lines.len() >= 3,
            "article has too few lines for multipart encrypted yEnc headers"
        );
        ensure!(
            lines[2].content.starts_with(b"=yencryption"),
            "restored line 3 must be =yencryption in multipart article"
        );
        2
    } else if lines[1].content.starts_with(b"=yencryption") {
        let is_multipart = lines[0]
            .content
            .windows(5)
            .position(|w| w == b"name=")
            .map(|name_idx| {
                let kv_prefix = &lines[0].content[..name_idx];
                kv_prefix
                    .split(|&b| b == b' ' || b == b'\t')
                    .any(|tok| tok.starts_with(b"part="))
            })
            .unwrap_or(false);
        ensure!(
            !is_multipart,
            "misplaced =yencryption: multipart =ybegin requires =ypart before =yencryption"
        );
        1
    } else {
        bail!("missing or misplaced =yencryption header line");
    };

    // Ensure no other =yencryption line exists in the entire article
    for (idx, line) in lines.iter().enumerate() {
        if idx != yenc_line_idx && line.content.starts_with(b"=yencryption") {
            bail!(
                "duplicate or misplaced =yencryption line at line {}",
                idx + 1
            );
        }
    }

    let params = parse_yencryption_line(lines[yenc_line_idx].content)?;

    let mut out = Vec::with_capacity(input.len());
    for (idx, line) in lines.iter().enumerate() {
        if idx != yenc_line_idx {
            out.extend_from_slice(line.content);
            out.extend_from_slice(line.ending);
        }
    }

    Ok((params, out))
}

/// Thin adapter presenting standard yEnc decode contract for downloaded articles.
pub struct DownloadDecryptionAdapter {
    password: Option<String>,
    cached_sessions: std::sync::Mutex<HashMap<[u8; 16], Arc<EncryptionSession>>>,
}

impl DownloadDecryptionAdapter {
    pub fn new(session: Option<Arc<EncryptionSession>>) -> Self {
        let mut map = HashMap::new();
        if let Some(s) = session {
            map.insert(s.salt(), s);
        }
        Self {
            password: None,
            cached_sessions: std::sync::Mutex::new(map),
        }
    }

    pub fn with_password(password: &str) -> Self {
        Self {
            password: Some(password.to_string()),
            cached_sessions: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Decode an article wire body, restoring encrypted control lines and decrypting body ciphertext.
    ///
    /// If the article is unencrypted, delegates directly to standard yEnc decoding.
    /// If encrypted, requires `segment_index` and valid session/password.
    /// Strictly guarantees Zero-Output on authentication failure.
    pub fn decode_article(&self, body: &[u8], segment_index: Option<u32>) -> Result<DecodedPart> {
        let Some(segment_index) = segment_index else {
            ensure!(
                body.starts_with(b"=ybegin"),
                "encrypted article received without segment identity"
            );
            return yenc::decode_part(body);
        };

        ensure!(segment_index > 0, "segment index must be greater than zero");
        ensure!(
            !body.starts_with(b"=ybegin"),
            "UNAUTHENTICATED_ARTICLE: unencrypted article received for encrypted segment"
        );

        let first_line_end = body
            .iter()
            .position(|&b| b == b'\n')
            .context("article has no newline terminators")?;
        let first_line = &body[..first_line_end]
            .strip_suffix(b"\r")
            .unwrap_or(&body[..first_line_end]);

        let salt = control::extract_salt_from_line1(first_line)?;
        let session = self.get_or_create_session(salt)?;

        // Decrypt control lines
        let restored_yenc = control::decrypt_yenc_control_lines(&session, segment_index, body)?;

        // Extract and remove =yencryption line
        let (yenc_params, clean_yenc) = extract_and_remove_yencryption(&restored_yenc)?;

        // Ensure salt agreement between control line 1 and =yencryption header
        ensure!(
            yenc_params.salt == session.salt(),
            "salt mismatch between control line 1 and =yencryption header"
        );

        // Decode yEnc ciphertext
        let mut decoded = yenc::decode_part(&clean_yenc)?;

        // Ciphertext CRC check before AEAD decryption
        ensure!(decoded.crc_matches(), "ciphertext CRC mismatch");

        // Authenticate and decrypt body ciphertext
        let nonce = session.derive_body_nonce(segment_index);
        let plaintext = body::decrypt_body(
            &decoded.data,
            &yenc_params.tag,
            session.master_key(),
            &nonce,
        )
        .map_err(|e| anyhow::anyhow!("AUTHENTICATION_FAILURE: {e}"))?;

        // Zero-output guarantee: clear ciphertext CRC metadata and replace data with authenticated plaintext
        decoded.part_crc32 = None;
        decoded.file_crc32 = None;
        decoded.data = plaintext;
        Ok(decoded)
    }

    /// Directly authenticate and decrypt ciphertext with given salt and segment index.
    ///
    /// Strictly guarantees Zero-Output on authentication failure.
    pub fn decrypt_raw(
        &self,
        salt: [u8; 16],
        segment_index: u32,
        ciphertext: &[u8],
        tag: &[u8; 16],
    ) -> Result<Vec<u8>> {
        let session = self.get_or_create_session(salt)?;
        let nonce = session.derive_body_nonce(segment_index);
        body::decrypt_body(ciphertext, tag, session.master_key(), &nonce)
            .map_err(|e| anyhow::anyhow!("AUTHENTICATION_FAILURE: {e}"))
    }

    fn get_or_create_session(&self, salt: [u8; 16]) -> Result<Arc<EncryptionSession>> {
        let mut guard = self.cached_sessions.lock().unwrap();
        if let Some(session) = guard.get(&salt) {
            return Ok(session.clone());
        }

        if let Some(ref pwd) = self.password {
            let session = Arc::new(EncryptionSession::new(pwd, salt)?);
            guard.insert(salt, session.clone());
            return Ok(session);
        }

        bail!("no password or encryption session available to decrypt article");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multi_salt_session_caching() {
        let password = "multi-salt-cache-password";
        let adapter = DownloadDecryptionAdapter::with_password(password);

        let salt1 = [0x11u8; 16];
        let salt2 = [0x22u8; 16];

        let s1 = adapter.get_or_create_session(salt1).unwrap();
        let s2 = adapter.get_or_create_session(salt2).unwrap();
        assert_eq!(s1.salt(), salt1);
        assert_eq!(s2.salt(), salt2);
        assert!(!Arc::ptr_eq(&s1, &s2));

        // Re-request salt1: must return cached instance (same Arc pointer)
        let s1_again = adapter.get_or_create_session(salt1).unwrap();
        assert!(Arc::ptr_eq(&s1, &s1_again));

        // Re-request salt2: must return cached instance (same Arc pointer)
        let s2_again = adapter.get_or_create_session(salt2).unwrap();
        assert!(Arc::ptr_eq(&s2, &s2_again));
    }

    #[test]
    fn test_single_part_filename_containing_part_equals() {
        let password = "test-part-equals-password";
        let salt = control::generate_alphabet_salt();
        let session = Arc::new(EncryptionSession::new(password, salt).unwrap());
        let upload_adapter = UploadEncryptionAdapter::new(session);

        let payload = b"Content for file named ep_part=pilot.mkv";
        let spec = PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        };
        let identity = SegmentIdentity::checked(0, 1, 1, 1).unwrap();
        let mut body = Vec::new();
        let encoded = upload_adapter
            .encode_article(
                "ep_part=pilot.mkv",
                payload.len() as u64,
                spec,
                payload,
                128,
                None,
                identity,
                &mut body,
            )
            .expect("encode single-part file with part= in filename");

        let download_adapter = DownloadDecryptionAdapter::with_password(password);
        let decoded = download_adapter
            .decode_article(&encoded.body, Some(identity.segment_index))
            .expect("decode single-part file with part= in filename must succeed");

        assert_eq!(decoded.data, payload);
        assert_eq!(decoded.name, "ep_part=pilot.mkv");
    }

    #[test]
    fn test_multipart_filename_containing_ypart() {
        let password = "test-multipart-ypart-password";
        let salt = control::generate_alphabet_salt();
        let session = Arc::new(EncryptionSession::new(password, salt).unwrap());
        let upload_adapter = UploadEncryptionAdapter::new(session);

        let payload = vec![0x33u8; 1024];
        let spec = PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        };
        let identity = SegmentIdentity::checked(0, 1, 2, 1).unwrap();
        let mut body = Vec::new();
        let encoded = upload_adapter
            .encode_article(
                "movie=ypart1.mkv",
                2048,
                spec,
                &payload,
                128,
                None,
                identity,
                &mut body,
            )
            .expect("encode multipart file with =ypart in filename");

        let download_adapter = DownloadDecryptionAdapter::with_password(password);
        let decoded = download_adapter
            .decode_article(&encoded.body, Some(identity.segment_index))
            .expect("decode multipart file with =ypart in filename must succeed");

        assert_eq!(decoded.data, payload);
        assert_eq!(decoded.name, "movie=ypart1.mkv");
        assert_eq!(decoded.part, 1);
        assert_eq!(decoded.total, 2);
    }
}
