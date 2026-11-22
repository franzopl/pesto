use super::*;

// ── par2_output_dir ───────────────────────────────────────────────────────

#[test]
fn par2_output_dir_loose_file_is_parent_dir() {
    // A single-component name like "movie.mkv" lives directly next to the file.
    let path = std::path::PathBuf::from("/data/movie.mkv");
    let meta = meta_with_name(&path, "movie.mkv");
    assert_eq!(par2_output_dir(&meta), std::path::Path::new("/data"));
}

#[test]
fn par2_output_dir_nested_file_strips_depth() {
    // "Season01/ep01.mkv" has depth 2, so par2 dir is 2 levels up.
    let path = std::path::PathBuf::from("/data/Season01/ep01.mkv");
    let meta = meta_with_name(&path, "Season01/ep01.mkv");
    assert_eq!(par2_output_dir(&meta), std::path::Path::new("/data"));
}

#[test]
fn par2_output_dir_three_levels_deep() {
    let path = std::path::PathBuf::from("/srv/a/b/c.bin");
    let meta = meta_with_name(&path, "a/b/c.bin");
    assert_eq!(par2_output_dir(&meta), std::path::Path::new("/srv"));
}

#[test]
fn par2_layout_prediction_matches_parmesan_output() {
    use parmesan::packet::{self, SliceChecksum};
    for slice_size in [64usize, 2048, 4096] {
        for file_count in [1usize, 2, 5] {
            let metas: Vec<_> = (0..file_count)
                .map(|index| {
                    let name = format!("nested/file{index}.bin");
                    let mut meta = meta_with_name(std::path::Path::new(&name), &name);
                    meta.size = (index * slice_size + 17) as u64;
                    Arc::new(meta)
                })
                .collect();
            let ids: Vec<_> = (0..file_count).map(|index| [index as u8; 16]).collect();
            let main = packet::main_body(slice_size as u64, &ids);
            let rsid = packet::recovery_set_id(&main);
            let mut packets = packet::serialize_packet(&rsid, &packet::TYPE_MAIN, &main);
            packets.extend(packet::serialize_packet(
                &rsid,
                &packet::TYPE_CREATOR,
                &packet::creator_body("pesto"),
            ));
            for (meta, id) in metas.iter().zip(&ids) {
                packets.extend(packet::serialize_packet(
                    &rsid,
                    &packet::TYPE_FILE_DESC,
                    &packet::file_description_body(
                        id,
                        &[0; 16],
                        &[0; 16],
                        meta.size,
                        &meta.client_path,
                    ),
                ));
                let checksums = vec![
                    SliceChecksum {
                        md5: [0; 16],
                        crc32: 0
                    };
                    (meta.size as usize).div_ceil(slice_size)
                ];
                packets.extend(packet::serialize_packet(
                    &rsid,
                    &packet::TYPE_IFSC,
                    &packet::ifsc_body(id, &checksums),
                ));
            }
            let predicted = prepare::par2_base_packets_len(&metas, slice_size);
            assert_eq!(predicted, packets.len() as u64);
            for exp in 0..5 {
                packets.extend(packet::serialize_packet(
                    &rsid,
                    &packet::TYPE_RECOVERY,
                    &packet::recovery_body(exp, &vec![0; slice_size]),
                ));
            }
            assert_eq!(
                prepare::par2_volume_len(predicted, 5, slice_size),
                packets.len() as u64
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn par2_base_prediction_matches_created_index_files() {
    use parmesan::create::{create, CreateRequest, Recovery, SliceStrategy};
    for slice_size in [64usize, 2048] {
        for file_count in [1usize, 3] {
            let dir = TempDir::new().unwrap();
            let paths: Vec<_> = (0..file_count)
                .map(|index| {
                    let path = dir.path().join(format!("file{index}.bin"));
                    std::fs::write(&path, vec![42; index * slice_size + 17]).unwrap();
                    path
                })
                .collect();
            let metas: Vec<_> = paths
                .iter()
                .enumerate()
                .map(|(index, path)| {
                    let mut meta = meta_with_name(path, &format!("file{index}.bin"));
                    meta.size = std::fs::metadata(path).unwrap().len();
                    Arc::new(meta)
                })
                .collect();
            let report = create(
                CreateRequest::from_paths(&paths)
                    .output_dir(dir.path().join("par2"))
                    .base_name("fixture")
                    .creator("pesto")
                    .recovery(Recovery::Blocks(5))
                    .slice_strategy(SliceStrategy::Size(slice_size)),
            )
            .await
            .unwrap();
            let base = prepare::par2_base_packets_len(&metas, slice_size);
            assert_eq!(
                std::fs::metadata(report.index_path.unwrap()).unwrap().len(),
                base
            );
        }
    }
}
