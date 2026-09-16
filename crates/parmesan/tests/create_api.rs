//! Public high-level creation API integration coverage.

use parmesan::create::{create, CreateRequest, Recovery, SliceStrategy};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::Command;

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "parmesan-create-api-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        Self(directory)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn creates_a_recursive_multi_file_set_with_empty_and_exact_slice_inputs() {
    let scratch = ScratchDir::new();
    let source = scratch.0.join("source");
    let nested = source.join("nested");
    let output = scratch.0.join("output");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(source.join("aligned.bin"), [0x5a; 64]).unwrap();
    std::fs::write(source.join("empty.nfo"), []).unwrap();
    std::fs::write(nested.join("tail.bin"), b"a partial final slice").unwrap();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let report = runtime
        .block_on(create(
            CreateRequest::from_paths([&source])
                .recurse()
                .output_dir(&output)
                .base_name("fixture")
                .recovery(Recovery::Blocks(2))
                .slice_strategy(SliceStrategy::Size(64))
                .threads(NonZeroUsize::new(2).unwrap()),
        ))
        .unwrap();

    assert_eq!(report.geometry.slice_size, 64);
    assert_eq!(report.geometry.recovery_blocks, 2);
    assert_eq!(report.index_path, Some(output.join("fixture.par2")));
    assert_eq!(
        report.volume_paths,
        [
            output.join("fixture.vol000+001.par2"),
            output.join("fixture.vol001+001.par2"),
        ]
    );
    assert!(report.index_path.as_ref().unwrap().is_file());
    assert!(report.volume_paths.iter().all(|path| path.is_file()));
}

#[test]
fn public_api_and_cli_create_byte_identical_output() {
    let scratch = ScratchDir::new();
    let first = scratch.0.join("first.bin");
    let second = scratch.0.join("second.bin");
    let api_output = scratch.0.join("api");
    let cli_output = scratch.0.join("cli");
    std::fs::write(&first, [0x91; 64]).unwrap();
    std::fs::write(&second, b"a second file with a partial slice").unwrap();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let api_report = runtime
        .block_on(create(
            CreateRequest::from_paths([&first, &second])
                .output_dir(&api_output)
                .base_name("fixture")
                .creator("parmesan")
                .recovery(Recovery::Blocks(2))
                .slice_strategy(SliceStrategy::Size(64)),
        ))
        .unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_parmesan"))
        .args([
            "create",
            first.to_str().unwrap(),
            second.to_str().unwrap(),
            "--recovery-count",
            "2",
            "--slice-size",
            "64",
            "--out-dir",
            cli_output.to_str().unwrap(),
            "--base-name",
            "fixture",
            "--quiet",
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let mut api_paths = api_report.volume_paths;
    api_paths.push(api_report.index_path.unwrap());
    for api_path in api_paths {
        let cli_path = cli_output.join(api_path.file_name().unwrap());
        assert_eq!(
            std::fs::read(&api_path).unwrap(),
            std::fs::read(&cli_path).unwrap(),
            "{} differs",
            api_path.file_name().unwrap().to_string_lossy()
        );
    }
}

#[test]
fn supports_memory_limited_passes_and_recovery_offsets() {
    let scratch = ScratchDir::new();
    let input = scratch.0.join("input.bin");
    let output = scratch.0.join("output");
    std::fs::write(&input, vec![0x27; 8192]).unwrap();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let report = runtime
        .block_on(create(
            CreateRequest::from_paths([&input])
                .output_dir(&output)
                .base_name("fixture")
                .recovery(Recovery::Blocks(4))
                .slice_strategy(SliceStrategy::Size(4096))
                .memory_limit(4096)
                .recovery_offset(7),
        ))
        .unwrap();

    assert_eq!(report.geometry.recovery_blocks, 4);
    assert_eq!(report.volume_paths.len(), 3);

    let mut exponents = Vec::new();
    for path in &report.volume_paths {
        let bytes = std::fs::read(path).unwrap();
        for packet in parmesan::packet_reader::read_packets(&bytes) {
            if packet.packet_type == parmesan::packet::TYPE_RECOVERY {
                exponents.push(u32::from_le_bytes(packet.body[..4].try_into().unwrap()));
            }
        }
    }
    exponents.sort_unstable();
    assert_eq!(exponents, [7, 8, 9, 10]);
}
