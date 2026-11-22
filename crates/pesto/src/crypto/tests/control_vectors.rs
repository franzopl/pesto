use super::*;

#[test]
fn test_control_line_encryption_test_vectors() {
    let path = test_vectors_dir().join("control_line_encryption.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: ControlLineFixture = serde_json::from_str(&content).unwrap();

    for vec in &fixture.vectors {
        let id = vec["id"].as_str().unwrap();

        if let Some(plaintext_line) = vec.get("plaintext_line").and_then(|v| v.as_str()) {
            let enc_key_bytes = fixture_bytes(vec, "derived_enc_key");
            let mut enc_key = [0u8; 32];
            enc_key.copy_from_slice(&enc_key_bytes);

            let tweak_bytes = fixture_bytes(vec, "derived_tweak");
            let mut tweak = [0u8; 8];
            tweak.copy_from_slice(&tweak_bytes);

            let pt_bytes = plaintext_line.as_bytes();
            let ct_bytes = ff1_encrypt_line(&enc_key, &tweak, pt_bytes)
                .unwrap_or_else(|e| panic!("ff1 encrypt failed for vector {id}: {e}"));

            let is_line_1 = vec["is_line_1"].as_bool().unwrap_or(false);
            let wire_bytes = if is_line_1 {
                let expected_wire = fixture_bytes(vec, "expected_wire");
                let bootstrap = &expected_wire[0..control::BOOTSTRAP_PREFIX_LEN];
                [bootstrap, &ct_bytes].concat()
            } else {
                ct_bytes.clone()
            };

            let expected_wire = fixture_bytes(vec, "expected_wire");
            assert_eq!(
                wire_bytes, expected_wire,
                "control line wire mismatch for vector {id}"
            );

            // Decrypt
            let restored_pt = ff1_decrypt_line(&enc_key, &tweak, &ct_bytes)
                .unwrap_or_else(|e| panic!("ff1 decrypt failed for vector {id}: {e}"));
            assert_eq!(
                restored_pt, pt_bytes,
                "control line roundtrip mismatch for vector {id}"
            );
        } else if let Some(input_lines_val) = vec.get("input_lines").and_then(|v| v.as_array()) {
            let lines: Vec<&str> = input_lines_val
                .iter()
                .map(|l| l.as_str().unwrap())
                .collect();
            let expected_wire_lines = fixture_bytes_list(vec, "expected_wire_lines");

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
            let segment_index = vec["segment_index"].as_u64().unwrap() as u32;

            let joined = lines.join("\r\n") + "\r\n";
            let encrypted_wire =
                control::encrypt_yenc_control_lines(&session, segment_index, joined.as_bytes())
                    .unwrap_or_else(|e| panic!("encrypt_yenc_control_lines failed for {id}: {e}"));

            let split_wire = control::split_lines_preserving_endings(&encrypted_wire);
            assert_eq!(
                split_wire.len(),
                expected_wire_lines.len(),
                "line count mismatch for {id}"
            );
            for (actual, expected) in split_wire.iter().zip(&expected_wire_lines) {
                assert_eq!(
                    actual.content, *expected,
                    "wire line bytes mismatch in {id}"
                );
            }

            // Vector ids ending in `-data` without an =yencryption line are
            // control-only articles — out of scope for v1.2 combined-only
            // reception (Control Std §5 step 4e); decryption legitimately
            // fails closed. Only combined-mode vectors must round-trip.
            let is_control_only = lines.iter().all(|l| !l.starts_with("=yencryption"));
            if is_control_only {
                assert!(
                    control::decrypt_yenc_control_lines(&session, segment_index, &encrypted_wire)
                        .is_err(),
                    "control-only vector {id} must fail closed under combined-only reception"
                );
                continue;
            }

            let decrypted =
                control::decrypt_yenc_control_lines(&session, segment_index, &encrypted_wire)
                    .unwrap_or_else(|e| panic!("decrypt_yenc_control_lines failed for {id}: {e}"));
            assert_eq!(
                decrypted,
                joined.as_bytes(),
                "full article roundtrip failed for {id}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    #[test]
    fn control_line_roundtrip_preserves_framing_for_arbitrary_session_identity(
        password in "[ -~]{1,32}",
        salt_numerals in prop::array::uniform16(0u16..253),
        // Index framing rule: the round-trip must exercise only wire-safe indices (the
        // uploader allocator never assigns forbidden ones); arbitrary u32
        // values including forbidden indices are rejected at the bootstrap.
        segment_index in (1u32..=200u32)
            .prop_filter("segment_index must satisfy the index framing rule", |i| {
                i.to_be_bytes().iter().all(|&b| b != 0x0A && b != 0x0D)
            }),
        name in "[A-Za-z0-9._-]{1,48}",
        payload in prop::collection::vec(any::<u8>(), 0..512),
        line_len in 1usize..129,
    ) {
        let mut salt = [0u8; 16];
        for (byte, numeral) in salt.iter_mut().zip(salt_numerals) {
            *byte = control::numeral_to_byte(numeral).unwrap();
        }
        let session = EncryptionSession::new(&password, salt).unwrap();
        let mut yenc = Vec::new();
        let spec = PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        };
        let encoded = crate::yenc::encode_part_into(
            &name,
            payload.len().max(1) as u64,
            spec,
            &payload,
            line_len,
            None,
            &mut yenc,
        );
        let insertion = encoded.body
            .windows(7)
            .position(|window| window == b"\n=ypart")
            .map(|position| position + 1)
            .unwrap();
        let insertion = insertion
            + encoded.body[insertion..]
                .iter()
                .position(|&byte| byte == b'\n')
                .unwrap()
            + 1;
        let encryption_line = format!(
            "=yencryption cipher=XChaCha20-Poly1305 salt={} tag=00000000000000000000000000000000\r\n",
            hex_encode(&salt),
        );
        let mut plaintext = Vec::new();
        plaintext.extend_from_slice(&encoded.body[..insertion]);
        plaintext.extend_from_slice(encryption_line.as_bytes());
        plaintext.extend_from_slice(&encoded.body[insertion..]);

        let wire = control::encrypt_yenc_control_lines(&session, segment_index, &plaintext).unwrap();
        let restored = control::decrypt_yenc_control_lines(&session, segment_index, &wire).unwrap();

        prop_assert_eq!(restored, plaintext);
    }
}
