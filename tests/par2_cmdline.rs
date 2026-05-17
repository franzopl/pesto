use pesto::par2::encoder::{slice_checksum, FileHasher, RecoveryEncoder};
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
        encoder.add_slice(&padded);
    }

    let recovery_slices = encoder.finish();

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
    let status = Command::new("par2")
        .arg("verify")
        .arg("-q")
        .arg(&index_path)
        .current_dir(&dir)
        .status();

    // Skip the test if `par2` is not installed or errors starting
    if let Ok(st) = status {
        assert!(st.success(), "par2cmdline verify failed on pristine files");
    } else {
        println!("par2cmdline not found, skipping validation test");
        return;
    }

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
