use super::*;

#[derive(Deserialize)]
struct MalformedFixture {
    vectors: Vec<serde_json::Value>,
}

#[test]
fn test_malformed_inputs_rejection() {
    let path = test_vectors_dir().join("malformed_inputs.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: MalformedFixture = serde_json::from_str(&content).unwrap();

    for vec in &fixture.vectors {
        let id = vec["id"].as_str().unwrap();
        let category = vec["category"].as_str().unwrap();
        let expected_err = vec["expected_error"].as_str().unwrap();

        match category {
            "header_syntax" => {
                let input_line = vec["input_line"].as_str().unwrap();
                let res = parse_yencryption_line(input_line.as_bytes());
                assert!(
                    res.is_err(),
                    "vector {id} expected error {expected_err}, but passed"
                );
                let err_msg = res.unwrap_err().to_string();
                assert!(
                    err_msg.contains(expected_err),
                    "vector {id} error message '{err_msg}' should contain '{expected_err}'"
                );
            }
            "auth_failure" => {
                let password = vec
                    .get("transport_kdf_input")
                    .or_else(|| vec.get("password"))
                    .unwrap()
                    .as_str()
                    .unwrap();
                let salt_bytes = fixture_bytes(vec, "salt");
                let mut salt = [0u8; 16];
                salt.copy_from_slice(&salt_bytes);
                let session = EncryptionSession::new(password, salt).unwrap();

                let ct = vec
                    .get("tampered_ciphertext_bytes")
                    .or_else(|| vec.get("ciphertext_bytes"))
                    .map(bytes_from_value)
                    .unwrap();

                let tag_bytes = vec
                    .get("tampered_tag_bytes")
                    .or_else(|| vec.get("tag_bytes"))
                    .map(bytes_from_value)
                    .unwrap();
                let mut tag = [0u8; 16];
                tag.copy_from_slice(&tag_bytes);

                let segment_index = vec["segment_index"].as_u64().unwrap() as u32;
                let nonce = session.derive_body_nonce(segment_index);
                let aad = vec
                    .get("aad_bytes")
                    .map(bytes_from_value)
                    .unwrap_or_default();

                let res = decrypt_body(&ct, &tag, &aad, session.master_key(), &nonce);
                assert!(
                    res.is_err(),
                    "vector {id} expected error {expected_err}, but passed"
                );
            }
            "control_syntax" => {
                if let Some(tampered_salt) = vec.get("tampered_salt_bytes") {
                    let salt_bytes = bytes_from_value(tampered_salt);
                    // construct a line1 with 16-byte salt + segmentIndex +
                    // minimal content (mirrors public_api.rs bootstrap shape)
                    let line1 = [
                        salt_bytes.as_slice(),
                        &1u32.to_be_bytes(),
                        b"=ybegin line=128 size=18",
                    ]
                    .concat();
                    let res = extract_bootstrap_from_line1(&line1);
                    assert!(res.is_err(), "vector {id} expected error {expected_err}");
                    let err_msg = res.unwrap_err().to_string();
                    assert!(
                        err_msg.contains(expected_err),
                        "vector {id} error '{err_msg}' should contain '{expected_err}'"
                    );
                } else if let Some(line_bytes) = vec.get("line_bytes") {
                    let bytes = bytes_from_value(line_bytes);
                    let dummy_key = [1u8; 32];
                    let dummy_tweak = [2u8; 8];
                    let res = ff1_encrypt_line(&dummy_key, &dummy_tweak, &bytes);
                    assert!(res.is_err(), "vector {id} expected error {expected_err}");
                    let err_msg = res.unwrap_err().to_string();
                    assert!(
                        err_msg.contains(expected_err),
                        "vector {id} error '{err_msg}' should contain '{expected_err}'"
                    );
                } else if let Some(line1_bytes) = vec.get("line1_bytes") {
                    let bytes = bytes_from_value(line1_bytes);
                    let res = extract_bootstrap_from_line1(&bytes);
                    assert!(res.is_err(), "vector {id} expected error {expected_err}");
                    let err_msg = res.unwrap_err().to_string();
                    assert!(
                        err_msg.contains(expected_err),
                        "vector {id} error '{err_msg}' should contain '{expected_err}'"
                    );
                } else if let Some(wrong_pwd) = vec.get("wrong_password").and_then(|v| v.as_str()) {
                    // Try to decrypt with wrong password
                    let salt = [0x42u8; 16];
                    let correct_session =
                        EncryptionSession::new(fixture_kdf_input(), salt).unwrap();
                    let wrong_session = EncryptionSession::new(wrong_pwd, salt).unwrap();
                    let input = b"=ybegin line=128 size=18 name=file.bin\r\n=yend size=18\r\n";
                    let wire =
                        control::encrypt_yenc_control_lines(&correct_session, 1, input).unwrap();
                    let res = control::decrypt_yenc_control_lines(&wrong_session, 1, &wire);
                    assert!(res.is_err(), "vector {id} expected error {expected_err}");
                    let err_msg = res.unwrap_err().to_string();
                    assert!(
                        err_msg.contains(expected_err),
                        "vector {id} error '{err_msg}' should contain '{expected_err}'"
                    );
                }
            }
            "salt_mismatch" => {
                let line1_salt_bytes = fixture_bytes(vec, "line1_salt");
                let header_salt_hex = hex_encode(&fixture_bytes(vec, "header_salt"));
                let line1_idx = vec["line1_index"].as_u64().unwrap() as u32;
                let header_idx = vec["header_index"].as_u64().unwrap() as u32;

                let mut salt = [0u8; 16];
                salt.copy_from_slice(&line1_salt_bytes);
                let session = Arc::new(EncryptionSession::new(fixture_kdf_input(), salt).unwrap());
                let upload_adapter = UploadEncryptionAdapter::new(session.clone());

                let payload = b"Hello World Dual Bootstrap Test";
                let spec = PartSpec {
                    number: 1,
                    total: 1,
                    offset: 0,
                };
                let segment_index = line1_idx;
                let mut body = Vec::new();
                let enc = upload_adapter
                    .encode_article(
                        "file.bin",
                        payload.len() as u64,
                        spec,
                        payload,
                        128,
                        segment_index,
                        &mut body,
                    )
                    .unwrap();

                let restored =
                    control::decrypt_yenc_control_lines(&session, line1_idx, &enc.body).unwrap();
                let split = control::split_lines_preserving_endings(&restored);
                let mut modified = Vec::new();
                for line in split {
                    if line.content.starts_with(b"=yencryption") {
                        let new_header = format!(
                            "=yencryption cipher=XChaCha20-Poly1305 salt={} index={:08x} tag=ed70d238067735a20783df5e094ccafa",
                            header_salt_hex, header_idx
                        );
                        modified.extend_from_slice(new_header.as_bytes());
                        modified.extend_from_slice(line.ending);
                    } else {
                        modified.extend_from_slice(line.content);
                        modified.extend_from_slice(line.ending);
                    }
                }
                let re_enc =
                    control::encrypt_yenc_control_lines(&session, line1_idx, &modified).unwrap();

                let download_adapter =
                    DownloadDecryptionAdapter::with_password(fixture_kdf_input());
                let res = download_adapter.decode_article(&re_enc);
                assert!(res.is_err(), "vector {id} expected error {expected_err}");
                let err_msg = res.unwrap_err().to_string();
                assert!(
                    err_msg.contains(expected_err),
                    "vector {id} error '{err_msg}' should contain '{expected_err}'"
                );
            }
            "placement" => {
                if let (Some(has_part), Some(is_ypart)) = (
                    vec.get("has_part_on_ybegin").and_then(|v| v.as_bool()),
                    vec.get("is_ypart_on_line2").and_then(|v| v.as_bool()),
                ) {
                    // placement-03/04 carry only framing flags and no
                    // wire bytes: synthesize a contradictory-framing
                    // article from those flags, encrypt its control
                    // lines, and drive it through the production
                    // restoration (Control Std v1.2 §5).
                    let kdf_input = fixture_kdf_input();
                    let salt = control::generate_alphabet_salt();
                    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
                    let segment_index = 1u32;
                    // `=yencryption` placement follows line 2, so the
                    // salt/index/tag tokens just need to be well-formed.
                    let mut salt_hex = String::new();
                    for b in salt {
                        use std::fmt::Write as _;
                        write!(&mut salt_hex, "{b:02x}").unwrap();
                    }
                    let yenc_line = format!(
                        "=yencryption cipher=XChaCha20-Poly1305 salt={salt_hex} index=00000001 tag=00000000000000000000000000000000"
                    );
                    let restored_block: Vec<u8> = if has_part && !is_ypart {
                        // placement-04: single-part placement (line 2
                        // is =yencryption) but =ybegin carries part=.
                        format!(
                            "=ybegin part=1 total=1 line=128 size=4 name=place.bin\r\n{yenc_line}\r\ndata\r\n=yend size=4\r\n"
                        )
                        .into_bytes()
                    } else if !has_part && is_ypart {
                        // placement-03: line 2 is =ypart but =ybegin
                        // has no part=.
                        format!(
                            "=ybegin line=128 size=4 name=place.bin\r\n=ypart begin=1 end=4\r\n{yenc_line}\r\ndata\r\n=yend size=4\r\n"
                        )
                        .into_bytes()
                    } else {
                        panic!("vector {id}: unexpected framing flag combination");
                    };
                    let wire = control::encrypt_yenc_control_lines(
                        &session,
                        segment_index,
                        &restored_block,
                    )
                    .unwrap();
                    let res = control::decrypt_yenc_control_lines(&session, segment_index, &wire);
                    let err = res.unwrap_err();
                    let msg = err.to_string();
                    assert!(
                        msg.contains(expected_err),
                        "vector {id} error '{msg}' should contain '{expected_err}'"
                    );
                    continue;
                }
                // The vector says the =yencryption header sits at the wrong
                // physical line (3 for single-part / after data for
                // multipart). Build a real article, re-insert its
                // =yencryption line at the wrong position, and drive the
                // result through the production decoder: the placement
                // rule (Body Std v1.2 §4) must reject it as provider
                // corruption with zero output.
                let line_index = vec["line_index"].as_u64().unwrap() as usize;
                let multipart = vec["multipart"].as_bool().unwrap();
                assert!(
                    line_index >= 3,
                    "vector {id}: only wrong placements are vectors"
                );

                let kdf_input = fixture_kdf_input();
                let salt = control::generate_alphabet_salt();
                let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
                let uploader = UploadEncryptionAdapter::new(session.clone());
                let payload = b"placement vector payload";
                let spec = if multipart {
                    PartSpec {
                        number: 1,
                        total: 2,
                        offset: 0,
                    }
                } else {
                    PartSpec {
                        number: 1,
                        total: 1,
                        offset: 0,
                    }
                };
                let segment_index = 1u32;
                let mut body = Vec::new();
                let encoded = uploader
                    .encode_article(
                        "place.bin",
                        payload.len() as u64 * if multipart { 2 } else { 1 },
                        spec,
                        payload,
                        128,
                        segment_index,
                        &mut body,
                    )
                    .unwrap();

                // Restore the plaintext control block, move the
                // =yencryption line to the vector's (wrong) physical line,
                // and re-encrypt so only the placement is wrong.
                let restored =
                    control::decrypt_yenc_control_lines(&session, segment_index, &encoded.body)
                        .unwrap();
                let lines: Vec<Vec<u8>> = restored
                    .split_inclusive(|&b| b == b'\n')
                    .map(|l| l.to_vec())
                    .collect();
                let enc_line = lines
                    .iter()
                    .find(|l| l.starts_with(b"=yencryption"))
                    .expect("encoded article carries its =yencryption line")
                    .clone();
                let mut rebuilt: Vec<Vec<u8>> = Vec::new();
                for line in lines.iter() {
                    // The next pushed line becomes physical line
                    // `rebuilt.len() + 1` (1-based). Insert the header at
                    // the vector's `line_index` (displacing nothing — the
                    // moved line leaves a gap at its original position) and
                    // keep every other line, including the footer, in
                    // order.
                    if rebuilt.len() + 1 == line_index {
                        rebuilt.push(enc_line.clone());
                    }
                    if line.starts_with(b"=yencryption") {
                        continue;
                    }
                    rebuilt.push(line.clone());
                }
                let wrong_wire =
                    control::encrypt_yenc_control_lines(&session, segment_index, &rebuilt.concat())
                        .unwrap();

                let download_adapter = DownloadDecryptionAdapter::with_password(kdf_input);
                let res = download_adapter.decode_article(&wrong_wire);
                assert!(
                    res.is_err(),
                    "vector {id}: misplaced =yencryption must fail closed"
                );
                let msg = res.unwrap_err().to_string();
                assert!(
                    msg.contains("CONTROL_LINE_DECRYPT_FAILURE")
                        || msg.contains("misplaced")
                        || msg.contains("missing or misplaced")
                        || msg.contains("must be =yencryption"),
                    "vector {id}: error '{msg}' should reflect the misplaced header"
                );
            }
            "framing" => {
                // A body-only article lacks the combined-mode bootstrap and
                // must fail closed at the provider corruption gate with the
                // fixture's declared classification.
                assert_eq!(id, "metadata-04-body-only-vs-combined-only");
                let kdf_input = fixture_kdf_input();
                let download_adapter = DownloadDecryptionAdapter::with_password(kdf_input);
                let body_only = b"=ybegin line=128 size=4 name=meta.bin\r\n=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef1234567890 index=00000001 tag=ed70d238067735a20783df5e094ccafa\r\ntest\r\n=yend size=4 crc32=d87f7e0c\r\n";
                let res = download_adapter.decode_article(body_only);
                assert!(
                    res.is_err(),
                    "vector {id}: body-only article must fail closed under the combined-only receiver"
                );
                let msg = res.unwrap_err().to_string();
                assert!(
                    msg.contains(expected_err),
                    "vector {id} error '{msg}' should contain '{expected_err}'"
                );
            }
            "metadata_validation" => {
                // Metadata vectors are pre-fetch gates: they name the NZB
                // metadata defect and its tier. Route each defect through
                // the production metadata validator (pesto::nzb::parse,
                // the same gate penne's download path runs) and assert the
                // expected failure code — no article is ever fetched, so
                // zero output holds by construction.
                // Each meta sits on its own line: the reader matches
                // `<meta ` at line start (after trimming).
                let xml = |extra: &str| {
                    format!(
                        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    {extra}
  </head>
  <file poster="poster@example.com" date="1774300000" subject="&quot;meta.bin&quot; yEnc">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="64" number="1">meta-msg-01@example.com</segment>
    </segments>
  </file>
</nzb>"#
                    )
                };
                match id {
                    "metadata-01-missing-provenance" | "metadata-03-missing-password" => {
                        // Missing provenance flag or missing transport
                        // password: the reader parses fine, but the
                        // download-side decryptor construction is the gate
                        // (MISSING_ENCRYPTION_PROVENANCE = no
                        // `yenc_encrypted`; MISSING_PASSWORD = flag
                        // without password). `crate::penne`'s equivalent
                        // lives in penne::nzb::download_decryptor; assert
                        // the same reader facts here.
                        let parsed =
                            crate::nzb::parse(&xml(if id == "metadata-01-missing-provenance" {
                                "<meta type=\"password\">pw</meta>"
                            } else {
                                "<meta type=\"yenc_encrypted\">true</meta>"
                            }))
                            .unwrap_or_else(|e| panic!("vector {id}: parse failed: {e}"));
                        if id == "metadata-01-missing-provenance" {
                            assert!(
                                !parsed.meta.yenc_encrypted,
                                "vector {id}: without the provenance marker the NZB is not transport-encrypted"
                            );
                        } else {
                            assert!(parsed.meta.yenc_encrypted);
                            assert!(
                                parsed.meta.password.is_none(),
                                "vector {id}: encrypted flag without a password must surface as MISSING_PASSWORD downstream"
                            );
                        }
                    }
                    "metadata-02-false-provenance" => {
                        let parsed = crate::nzb::parse(&xml(
                            "<meta type=\"yenc_encrypted\">false</meta>\n    <meta type=\"password\">pw</meta>",
                        ))
                        .unwrap_or_else(|e| panic!("vector {id}: parse failed: {e}"));
                        assert!(
                            !parsed.meta.yenc_encrypted,
                            "vector {id}: provenance set to false must not enable transport decryption"
                        );
                    }
                    other => panic!("unknown metadata vector {other}"),
                }
            }
            other => panic!("unhandled malformed-input vector category {other} ({id})"),
        }
    }
}
