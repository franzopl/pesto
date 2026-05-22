use pesto::par2::encoder::{altmap_buffer_size, slice_checksum, FileHasher, RecoveryEncoder};
use pesto::par2::packet::{
    compute_file_id, creator_body, file_description_body, ifsc_body, main_body, recovery_body,
    recovery_set_id, serialize_packet, TYPE_CREATOR, TYPE_FILE_DESC, TYPE_IFSC, TYPE_MAIN,
    TYPE_RECOVERY,
};
use std::fs::File;
use std::io::Write;
use std::process::Command;

#[test]
fn generates_valid_par2_repaired_by_par2cmdline() {
    let dir = std::env::temp_dir().join(format!("pesto_par2_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let file_name = "test_file.bin";
    let file_path = dir.join(file_name);

    // Create random-ish data for the test file
    let mut original_data = Vec::new();
    for i in 0..1000 {
        original_data.push((i % 256) as u8);
    }
    std::fs::write(&file_path, &original_data).unwrap();

    // Config
    let slice_size = 400; // 3 slices for 1000 bytes
    let total_slices = 3;
    let recovery_count = 2; // Exponents 0, 1

    // 1. File hashing
    let mut hasher = FileHasher::new();
    hasher.update(&original_data);
    let hashes = hasher.finish();

    // 2. Slices and checksums
    let mut slice_checksums = Vec::new();
    let mut encoder = RecoveryEncoder::new(slice_size, total_slices, 0, recovery_count);

    for chunk in original_data.chunks(slice_size) {
        let mut padded = chunk.to_vec();
        padded.resize(slice_size, 0);
        slice_checksums.push(slice_checksum(&padded));
        encoder.add_slice(padded);
    }

    let (recovery_slices, _) = encoder.finish();

    // 3. Packets
    let file_id = compute_file_id(&hashes.md5_16k, hashes.length, file_name);

    let main_b = main_body(slice_size as u64, &[file_id]);
    let rsid = recovery_set_id(&main_b);
    let pkt_main = serialize_packet(&rsid, &TYPE_MAIN, &main_b);

    let pkt_creator = serialize_packet(&rsid, &TYPE_CREATOR, &creator_body("pesto test"));

    let pkt_file_desc = serialize_packet(
        &rsid,
        &TYPE_FILE_DESC,
        &file_description_body(
            &file_id,
            &hashes.md5_full,
            &hashes.md5_16k,
            hashes.length,
            file_name,
        ),
    );

    let pkt_ifsc = serialize_packet(&rsid, &TYPE_IFSC, &ifsc_body(&file_id, &slice_checksums));

    let mut pkt_recoveries = Vec::new();
    for (i, rec) in recovery_slices.iter().enumerate() {
        let rec_body = recovery_body(i as u32, &rec.data);
        pkt_recoveries.push(serialize_packet(&rsid, &TYPE_RECOVERY, &rec_body));
    }

    // Base packets
    let mut base_packets = Vec::new();
    base_packets.extend(&pkt_main);
    base_packets.extend(&pkt_creator);
    base_packets.extend(&pkt_file_desc);
    base_packets.extend(&pkt_ifsc);

    // Write index file
    let index_path = dir.join(format!("{}.par2", file_name));
    std::fs::write(&index_path, &base_packets).unwrap();

    // Write volume file
    let vol_path = dir.join(format!("{}.vol00+02.par2", file_name));
    let mut vol_file = File::create(&vol_path).unwrap();
    vol_file.write_all(&base_packets).unwrap();
    for rec_pkt in pkt_recoveries {
        vol_file.write_all(&rec_pkt).unwrap();
    }

    // Verify using par2cmdline
    let result = Command::new("par2")
        .arg("verify")
        .arg("-q")
        .arg(&index_path)
        .current_dir(&dir)
        .output();

    // Skip the test if `par2` is not installed or could not be executed.
    // Exit code 127 means "command not found" (set by the shell / exec failure).
    let st = match result {
        Err(_) => {
            println!("par2cmdline not found, skipping validation test");
            return;
        }
        Ok(out) if out.status.code() == Some(127) => {
            println!("par2cmdline not found (exit 127), skipping validation test");
            return;
        }
        Ok(out) => out.status,
    };
    assert!(st.success(), "par2cmdline verify failed on pristine files");

    // Corrupt the original file
    let mut corrupted = original_data.clone();
    corrupted[0] ^= 0xFF; // Flip bits
    std::fs::write(&file_path, &corrupted).unwrap();

    // Verify should fail now
    let verify_fail = Command::new("par2")
        .arg("verify")
        .arg("-q")
        .arg(&index_path)
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(
        !verify_fail.success(),
        "par2cmdline verify should fail after corruption"
    );

    // Repair should succeed
    let repair = Command::new("par2")
        .arg("repair")
        .arg("-q")
        .arg(&index_path)
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(repair.success(), "par2cmdline repair failed");

    // Check if the file is fixed
    let repaired_data = std::fs::read(&file_path).unwrap();
    assert_eq!(
        repaired_data, original_data,
        "file content was not correctly restored"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Same end-to-end repair test as above, but uses the ALTMAP encoder path
/// (Phase 27e/27f).  Verifies that the ALTMAP XOR bit-dependency kernel
/// produces recovery data that par2cmdline can use to repair a corrupted file.
#[test]
fn altmap_path_generates_valid_par2_repaired_by_par2cmdline() {
    // slice_size must be a multiple of 32 (= 16 u16 words) for ALTMAP.
    let slice_size = 512;
    let total_slices = 3;
    let recovery_count = 2;

    // Only meaningful on AVX2 hardware; skip otherwise to avoid the no-op drain path.
    #[cfg(target_arch = "x86_64")]
    if !std::is_x86_feature_detected!("avx2") {
        println!("AVX2 not available — skipping ALTMAP par2cmdline test");
        return;
    }

    let dir = std::env::temp_dir().join(format!("pesto_par2_altmap_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let file_name = "test_altmap.bin";
    let file_path = dir.join(file_name);

    let total_bytes = slice_size * total_slices;
    // Use an LCG so each byte is pseudo-random and each slice is distinct.
    // Identical slices would confuse par2cmdline's sliding-window block scan.
    let mut lcg: u64 = 0xDEAD_BEEF_CAFE_4567;
    let mut original_data: Vec<u8> = (0..total_bytes)
        .map(|_| {
            lcg = lcg
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (lcg >> 56) as u8
        })
        .collect();
    std::fs::write(&file_path, &original_data).unwrap();

    let mut hasher = FileHasher::new();
    hasher.update(&original_data);
    let hashes = hasher.finish();

    let _ = altmap_buffer_size(slice_size / 2); // assert alignment is correct

    let mut slice_checksums = Vec::new();
    let mut encoder = RecoveryEncoder::new_altmap(slice_size, total_slices, 0, recovery_count);

    for chunk in original_data.chunks(slice_size) {
        let mut padded = chunk.to_vec();
        padded.resize(slice_size, 0);
        slice_checksums.push(slice_checksum(&padded));
        encoder.add_slice(padded);
    }

    let (recovery_slices, _) = encoder.finish();

    let file_id = compute_file_id(&hashes.md5_16k, hashes.length, file_name);
    let main_b = main_body(slice_size as u64, &[file_id]);
    let rsid = recovery_set_id(&main_b);
    let pkt_main = serialize_packet(&rsid, &TYPE_MAIN, &main_b);
    let pkt_creator = serialize_packet(&rsid, &TYPE_CREATOR, &creator_body("pesto altmap test"));
    let pkt_file_desc = serialize_packet(
        &rsid,
        &TYPE_FILE_DESC,
        &file_description_body(
            &file_id,
            &hashes.md5_full,
            &hashes.md5_16k,
            hashes.length,
            file_name,
        ),
    );
    let pkt_ifsc = serialize_packet(&rsid, &TYPE_IFSC, &ifsc_body(&file_id, &slice_checksums));

    let mut base_packets = Vec::new();
    base_packets.extend(&pkt_main);
    base_packets.extend(&pkt_creator);
    base_packets.extend(&pkt_file_desc);
    base_packets.extend(&pkt_ifsc);

    let index_path = dir.join(format!("{}.par2", file_name));
    std::fs::write(&index_path, &base_packets).unwrap();

    let vol_path = dir.join(format!("{}.vol00+02.par2", file_name));
    let mut vol_file = File::create(&vol_path).unwrap();
    vol_file.write_all(&base_packets).unwrap();
    for rec in &recovery_slices {
        let rec_body = recovery_body(rec.exponent, &rec.data);
        vol_file
            .write_all(&serialize_packet(&rsid, &TYPE_RECOVERY, &rec_body))
            .unwrap();
    }

    let result = Command::new("par2")
        .arg("verify")
        .arg("-q")
        .arg(&index_path)
        .current_dir(&dir)
        .output();

    let st = match result {
        Err(_) => {
            println!("par2cmdline not found, skipping ALTMAP validation test");
            return;
        }
        Ok(out) if out.status.code() == Some(127) => {
            println!("par2cmdline not found (exit 127), skipping ALTMAP validation test");
            return;
        }
        Ok(out) => out.status,
    };
    assert!(
        st.success(),
        "par2cmdline verify failed on pristine ALTMAP-encoded files"
    );

    original_data[0] ^= 0xFF;
    std::fs::write(&file_path, &original_data).unwrap();

    let verify_fail = Command::new("par2")
        .arg("verify")
        .arg("-q")
        .arg(&index_path)
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(
        !verify_fail.success(),
        "par2cmdline verify should fail after corruption (ALTMAP path)"
    );

    let repair = Command::new("par2")
        .arg("repair")
        .arg("-q")
        .arg(&index_path)
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(
        repair.success(),
        "par2cmdline repair failed for ALTMAP-encoded PAR2"
    );

    original_data[0] ^= 0xFF; // undo the flip for comparison
    let repaired_data = std::fs::read(&file_path).unwrap();
    assert_eq!(
        repaired_data, original_data,
        "repaired file content does not match original (ALTMAP path)"
    );

    std::fs::remove_dir_all(&dir).ok();
}
