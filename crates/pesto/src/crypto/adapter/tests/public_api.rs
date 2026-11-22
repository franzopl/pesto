use super::*;

#[test]
fn test_generate_alphabet_salt_properties() {
    for _ in 0..1000 {
        let salt = control::generate_alphabet_salt();
        for &b in &salt {
            assert!(b != 0x00 && b != 0x0A && b != 0x0D);
            assert!(control::byte_to_numeral(b).is_ok());
        }
        let mut line1 = Vec::new();
        line1.extend_from_slice(&salt);
        line1.extend_from_slice(&1u32.to_be_bytes());
        line1.extend_from_slice(b"=ybegin part=1\r\n");
        let (extracted, extracted_index) =
            extract_bootstrap_from_line1(&line1).expect("extract salt should succeed");
        assert_eq!(extracted, salt);
        assert_eq!(extracted_index, 1);
    }
}

#[test]
fn test_adapter_strict_header_order() {
    let kdf_input = fixture_kdf_input();
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let upload_adapter = UploadEncryptionAdapter::new(session.clone());

    // 1. Single-part article: line 1 = =ybegin, line 2 = =yencryption
    let payload = b"Strict single part header verification payload";
    let spec_single = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };
    let segment_index_single = 1u32;
    let mut body_single = Vec::new();
    let enc_single = upload_adapter
        .encode_article(
            "single.bin",
            payload.len() as u64,
            spec_single,
            payload,
            128,
            segment_index_single,
            &mut body_single,
        )
        .unwrap();

    let download_adapter = DownloadDecryptionAdapter::with_password(kdf_input);
    let dec_single = download_adapter
        .decode_article(&enc_single.body)
        .expect("single-part decode must succeed");
    assert_eq!(dec_single.data, payload);
    // CRC normalization recomputes part_crc32 over the authenticated plaintext
    // (Body Standard §10, recompute variant); file_crc32 stays cleared.
    assert_eq!(dec_single.part_crc32, Some(yenc::crc32(payload)));
    assert_eq!(dec_single.file_crc32, None);

    // 2. Multipart article: line 1 = =ybegin, line 2 = =ypart, line 3 = =yencryption
    let spec_multi = PartSpec {
        number: 1,
        total: 2,
        offset: 0,
    };
    let segment_index_multi = 1u32;
    let mut body_multi = Vec::new();
    let enc_multi = upload_adapter
        .encode_article(
            "multi.bin",
            payload.len() as u64 * 2,
            spec_multi,
            payload,
            128,
            segment_index_multi,
            &mut body_multi,
        )
        .unwrap();

    let dec_multi = download_adapter
        .decode_article(&enc_multi.body)
        .expect("multi-part decode must succeed");
    assert_eq!(dec_multi.data, payload);
    assert_eq!(dec_multi.part_crc32, Some(yenc::crc32(payload)));
    assert_eq!(dec_multi.file_crc32, None);
}

#[test]
fn test_adapter_ciphertext_crc_validation() {
    let kdf_input = fixture_kdf_input();
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let upload_adapter = UploadEncryptionAdapter::new(session.clone());

    let payload = b"Ciphertext CRC validation payload 123456789";
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };
    let segment_index = 1;
    let mut body = Vec::new();
    let enc = upload_adapter
        .encode_article(
            "test.bin",
            payload.len() as u64,
            spec,
            payload,
            128,
            segment_index,
            &mut body,
        )
        .unwrap();

    // Decrypting the intact article works
    let download_adapter = DownloadDecryptionAdapter::with_password(kdf_input);
    let dec = download_adapter
        .decode_article(&enc.body)
        .expect("intact article decodes");
    assert_eq!(dec.data, payload);
    // Recompute variant: part_crc32 now covers the authenticated plaintext
    // (never the discarded ciphertext wire CRC); file_crc32 stays cleared.
    assert_eq!(dec.part_crc32, Some(yenc::crc32(payload)));
    assert_eq!(dec.file_crc32, None);

    // Tamper with the =yend crc in the wire
    // Note: line 1, 2, and =yend are FF1 encrypted.
    // If we decrypt the control lines, tamper the yend crc, and re-encrypt, we can verify that ciphertext CRC mismatch fails before body decryption.
    let restored = control::decrypt_yenc_control_lines(&session, segment_index, &enc.body).unwrap();
    let text = String::from_utf8_lossy(&restored);
    // Mutate the crc32= in =yend
    let tampered_text = if let Some(idx) = text.find("crc32=") {
        let mut t = text.into_owned();
        t.replace_range(idx + 6..idx + 7, "f");
        t
    } else {
        panic!("missing crc32 in yend");
    };
    let re_encrypted =
        control::encrypt_yenc_control_lines(&session, segment_index, tampered_text.as_bytes())
            .unwrap();
    let err = download_adapter.decode_article(&re_encrypted).unwrap_err();
    assert!(
        err.to_string().contains("ciphertext CRC mismatch")
            || err.to_string().contains("CRC")
            || err.to_string().contains("crc")
    );
}

#[test]
fn test_adapter_flipped_tag_with_intact_wire_crc_fails_authentication() {
    // True Poly1305 failure (Body Std v1.2 §5): flip one `tag=` hex digit
    // in the restored `=yencryption` line and re-encrypt the control
    // lines. Data lines and the footer wire CRC are untouched, so the
    // ciphertext CRC check passes and AEAD authentication is the failing
    // step — the error must be AUTHENTICATION_FAILURE, never released
    // plaintext. This construction needs the crate-private
    // restore/re-encrypt codec, so it lives here rather than in penne's
    // public-seam integration tests.
    let kdf_input = fixture_kdf_input();
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let upload_adapter = UploadEncryptionAdapter::new(session.clone());

    let payload = b"Payload whose authentication tag is tampered with valid CRC.";
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };
    let segment_index = 1;
    let mut body = Vec::new();
    let enc = upload_adapter
        .encode_article(
            "tagflip.bin",
            payload.len() as u64,
            spec,
            payload,
            128,
            segment_index,
            &mut body,
        )
        .unwrap();

    let download_adapter = DownloadDecryptionAdapter::with_password(kdf_input);
    assert_eq!(
        download_adapter.decode_article(&enc.body).unwrap().data,
        payload,
        "intact article must decode before tag tampering"
    );

    let restored = control::decrypt_yenc_control_lines(&session, segment_index, &enc.body).unwrap();
    // Flip exactly one hex digit of `tag=` inside the `=yencryption`
    // line, preserving the line's length and lowercase-hex grammar.
    let yenc_pos = restored
        .windows(b"=yencryption".len())
        .position(|w| w == b"=yencryption")
        .expect("restored article carries its =yencryption line");
    let tag_rel = restored[yenc_pos..]
        .windows(4)
        .position(|w| w == b"tag=")
        .expect("=yencryption line carries tag=");
    let digit_pos = yenc_pos + tag_rel + 4;
    let mut tampered = restored.clone();
    tampered[digit_pos] = if tampered[digit_pos] == b'0' {
        b'1'
    } else {
        b'0'
    };
    assert_eq!(tampered.len(), restored.len());
    assert_ne!(tampered, restored);

    let re_encrypted =
        control::encrypt_yenc_control_lines(&session, segment_index, &tampered).unwrap();
    let err = download_adapter.decode_article(&re_encrypted).unwrap_err();
    assert!(
        err.to_string().contains("AUTHENTICATION_FAILURE"),
        "flipped tag with intact wire CRC must fail Poly1305 authentication, got: {err}"
    );
}

#[test]
fn test_adapter_zero_output_rejection_matrix() {
    let kdf_input = fixture_kdf_input();
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let upload_adapter = UploadEncryptionAdapter::new(session.clone());

    let payload = b"Zero output guarantee test payload 12345";
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };
    let segment_index = 1;
    let mut body = Vec::new();
    let enc = upload_adapter
        .encode_article(
            "test.bin",
            payload.len() as u64,
            spec,
            payload,
            128,
            segment_index,
            &mut body,
        )
        .unwrap();

    let download_adapter = Arc::new(DownloadDecryptionAdapter::with_password(kdf_input));

    // 1. The article is self-describing: identity comes from its bootstrap
    let dec = download_adapter
        .decode_article(&enc.body)
        .expect("self-describing article decodes from its bootstrap");
    assert_eq!(dec.data, payload);

    // 2. Wrong password fails closed
    let wrong_password_adapter = DownloadDecryptionAdapter::with_password(other_kdf_input());
    assert!(wrong_password_adapter.decode_article(&enc.body).is_err());

    // 3. Truncated article body fails closed
    let half = &enc.body[..enc.body.len() / 2];
    assert!(download_adapter.decode_article(half).is_err());

    // 4. Salt mismatch between line 1 and =yencryption fails closed
    let restored = control::decrypt_yenc_control_lines(&session, segment_index, &enc.body).unwrap();
    let text = String::from_utf8_lossy(&restored);
    if let Some(idx) = text.find("salt=") {
        let mut t = text.into_owned();
        // Change one char in salt
        let replacement = if &t[idx + 5..idx + 6] == "a" {
            "b"
        } else {
            "a"
        };
        t.replace_range(idx + 5..idx + 6, replacement);
        let re_enc =
            control::encrypt_yenc_control_lines(&session, segment_index, t.as_bytes()).unwrap();
        let err = download_adapter.decode_article(&re_enc).unwrap_err();
        assert!(err.to_string().contains("salt mismatch"));
    }

    // 5. Concurrency & idempotency test
    let mut handles = Vec::new();
    for _ in 0..10 {
        let adapter_clone = download_adapter.clone();
        let wire = enc.body.clone();
        handles.push(std::thread::spawn(move || {
            let res = adapter_clone.decode_article(&wire).unwrap();
            assert_eq!(res.data, payload);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn truncated_wire_line1_is_not_split_on_stray_newline_bytes() {
    // An encrypted wire article whose total length is shorter
    // than the 20-byte bootstrap prefix must be treated as a single truncated
    // line 1, not split at a 0x0A byte that happens to fall inside the (absent)
    // prefix. Before the fix, `split_lines_preserving_endings` skipped the
    // prefix only when `input.len() >= BOOTSTRAP_PREFIX_LEN`, so a short
    // adversarial body containing an embedded LF fragmented into multiple
    // "lines" before `extract_bootstrap_from_line1` could see it.
    let mut short_wire = vec![0x41u8; 10];
    short_wire[4] = b'\n'; // stray LF inside the truncated prefix region
    short_wire[7] = b'\r';
    short_wire[8] = b'\n';

    let lines = control::split_lines_preserving_endings(&short_wire);
    assert_eq!(
        lines.len(),
        1,
        "short wire body must yield exactly one truncated line, got {} lines",
        lines.len()
    );
    assert_eq!(lines[0].content, &short_wire[..]);

    // The decoder surfaces the truncation as a clean LINE_TRUNCATED error.
    let err = control::extract_bootstrap_from_line1(lines[0].content).unwrap_err();
    assert!(
        err.to_string().contains("LINE_TRUNCATED"),
        "expected LINE_TRUNCATED, got: {err}"
    );
}

#[test]
fn bootstrap_extraction_rejects_forbidden_segment_index_bytes() {
    // Index framing rule (Control Std v1.2 §5/§8): indices 10, 13, 266 (0x0000010A), 269
    // (0x0000010D) contain 0x0A/0x0D in their big-endian encoding and are
    // rejected under FORBIDDEN_SEGMENT_INDEX_BYTE (maps to PROVIDER_FAILOVER).
    for idx in [10u32, 13, 266, 269] {
        let mut line1 = vec![0x41u8; 16]; // salt placeholder (no forbidden bytes)
        line1.extend_from_slice(&idx.to_be_bytes());
        line1.extend_from_slice(b"=="); // minimal trailer past the 22-byte minimum
        let err = control::extract_bootstrap_from_line1(&line1).unwrap_err();
        assert!(
            err.to_string().contains("FORBIDDEN_SEGMENT_INDEX_BYTE"),
            "index {idx} must be rejected with FORBIDDEN_SEGMENT_INDEX_BYTE, got: {err}"
        );
    }
    // Zero still rejected with its own token.
    let mut line1 = vec![0x41u8; 16];
    line1.extend_from_slice(&0u32.to_be_bytes());
    line1.extend_from_slice(b"==");
    let err = control::extract_bootstrap_from_line1(&line1).unwrap_err();
    assert!(err.to_string().contains("ZERO_SEGMENT_INDEX"), "got: {err}");

    // Neighboring safe indices still accepted.
    for idx in [9u32, 11, 12, 14, 265, 267, 268, 270] {
        let mut line1 = vec![0x41u8; 16];
        line1.extend_from_slice(&idx.to_be_bytes());
        line1.extend_from_slice(b"==");
        let (_, extracted) = control::extract_bootstrap_from_line1(&line1).unwrap();
        assert_eq!(extracted, idx);
    }
}

#[test]
fn header_loop_ff1_error_fails_closed_not_passthrough() {
    // Control Std v1.2 §5 step 4: an FF1 decryption error on a line in
    // the expected-header region is PROVIDER_FAILOVER — it must never be
    // committed as a data line.
    let kdf_input = fixture_kdf_input();
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let segment_index = 7u32;

    // Build a valid encrypted article, then corrupt line 2 (the =ypart line
    // region — for multipart, line 2 is a header line) so its FF1 decryption
    // fails.
    let payload = b"fail closed payload bytes";
    let uploader = UploadEncryptionAdapter::new(session.clone());
    let mut body = Vec::new();
    let encoded = uploader
        .encode_article(
            "fc.bin",
            payload.len() as u64,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            payload,
            128,
            segment_index,
            &mut body,
        )
        .unwrap();

    let lines: Vec<Vec<u8>> = control::split_lines_preserving_endings(&encoded.body)
        .into_iter()
        .map(|l| [l.content.to_vec(), l.ending.to_vec()].concat())
        .collect();
    assert!(lines.len() >= 3, "need header + data + footer");

    // Case A: line 2 carries a byte OUTSIDE the Radix-253 Alphabet (0x00) —
    // byte_to_numeral fails inside ff1_decrypt_line, a genuine FF1 error on
    // an expected-header line. Must bail PROVIDER_FAILOVER, never become a
    // data line.
    let mut case_a = lines.clone();
    let pos = case_a[1]
        .iter()
        .position(|&b| b != b'\r' && b != b'\n')
        .unwrap();
    case_a[1][pos] = 0x00;
    let err =
        control::decrypt_yenc_control_lines(&session, segment_index, &case_a.concat()).unwrap_err();
    assert!(
        err.to_string()
            .contains("control-line decryption failed at line 2 in the header region"),
        "out-of-Alphabet header byte must fail closed, got: {err:#}"
    );

    // Case B: a bit flip in the header ciphertext decrypts "successfully" to
    // garbage (FF1 is a permutation) — the probe treats it as the first data
    // line, but the restored block then lacks its =yencryption header, so the
    // ADAPTER fails closed and releases zero output.
    let mut case_b = lines.clone();
    let flip = case_b[1]
        .iter()
        .position(|&b| b != b'\r' && b != b'\n')
        .unwrap();
    case_b[1][flip] ^= 0x01;
    let corrupted = case_b.concat();
    let adapter = DownloadDecryptionAdapter::with_password(kdf_input);
    let res = adapter.decode_article(&corrupted);
    assert!(
        res.is_err(),
        "corrupted =yencryption header must fail closed"
    );
    let msg = res.unwrap_err().to_string();
    // Zero-output: the error must not carry the decrypted data line content.
    let data_line = &lines[2];
    let probe = &data_line[..data_line.len().min(16)];
    assert!(
        !msg.as_bytes().windows(probe.len()).any(|w| w == probe),
        "error must not leak passthrough data"
    );
}

#[test]
fn encrypted_segments_normalize_crc_metadata_regression() {
    // Regression: after authentication and
    // decryption, DecodedPart must NOT carry the ciphertext wire CRC into any
    // verification path — the ciphertext `crc32=`/`pcrc32=` values are
    // discarded and part_crc32 is RECOMPUTED over the authenticated plaintext
    // (Body Standard §10, recompute variant); file_crc32 is None (the
    // whole-file CRC cannot be derived from a single segment).
    let kdf_input = fixture_kdf_input();
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let payload = b"ciphertext CRC must never leak into verification";
    let uploader = UploadEncryptionAdapter::new(session.clone());
    let mut body = Vec::new();
    let segment_index = 1;
    let encoded = uploader
        .encode_article(
            "crc.bin",
            payload.len() as u64,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            payload,
            128, // wire =yend carries a (ciphertext) file CRC
            segment_index,
            &mut body,
        )
        .unwrap();

    let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
        .decode_article(&encoded.body)
        .expect("authenticated decode must succeed");
    assert_eq!(
        decoded.part_crc32,
        Some(yenc::crc32(payload)),
        "part_crc32 must be recomputed over the authenticated plaintext"
    );
    assert!(
        decoded.file_crc32.is_none(),
        "file_crc32 must be None for encrypted segments"
    );
    assert_eq!(decoded.data, payload);
}
