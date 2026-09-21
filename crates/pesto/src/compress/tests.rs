use super::*;

#[test]
fn random_password_is_24_alphanumeric_chars() {
    let p = random_password();
    assert_eq!(p.len(), 24);
    assert!(
        p.chars().all(|c| c.is_ascii_alphanumeric()),
        "non-alphanumeric: {p}"
    );
}

#[test]
fn random_passwords_are_not_identical() {
    // Two calls in the same process should differ (LCG advances seed).
    let a = random_password();
    let b = random_password();
    assert_ne!(a, b);
}

#[test]
fn list_matching_returns_unpadded_rar_volumes_in_volume_order() {
    let dir = std::env::temp_dir().join(format!("pesto_list_matching_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for n in [1, 2, 3, 10, 11, 12] {
        std::fs::write(dir.join(format!("stem.part{n}.rar")), b"x").unwrap();
    }

    let found = list_matching(&dir, |name| {
        name.starts_with("stem.part") && name.ends_with(".rar")
    })
    .unwrap();
    let names: Vec<String> = found
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "stem.part1.rar",
            "stem.part2.rar",
            "stem.part3.rar",
            "stem.part10.rar",
            "stem.part11.rar",
            "stem.part12.rar",
        ]
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn volume_suffix_extracts_rar_part_pattern() {
    assert_eq!(
        volume_suffix("v0ZAlTJxqARhGwW4tH2.part07.rar"),
        Some(".part07.rar")
    );
    assert_eq!(
        volume_suffix("v0ZAlTJxqARhGwW4tH2.part001.rar"),
        Some(".part001.rar")
    );
}

#[test]
fn volume_suffix_extracts_7z_numeric_pattern() {
    assert_eq!(volume_suffix("v0ZAlTJxqARhGwW4tH2.7z.001"), Some(".7z.001"));
}

#[test]
fn volume_suffix_is_none_for_unrelated_names() {
    for name in [
        "v0ZAlTJxqARhGwW4tH2.rar",
        "v0ZAlTJxqARhGwW4tH2.7z",
        "v0ZAlTJxqARhGwW4tH2.par2",
        "v0ZAlTJxqARhGwW4tH2.vol000+001.par2",
        "movie.partial.rar",
        "movie.part.rar",
    ] {
        assert_eq!(volume_suffix(name), None, "expected `{name}` to be None");
    }
}

#[test]
fn client_archive_name_hides_private_scratch_stem() {
    for (physical, expected) in [
        ("random.7z", "Release.7z"),
        ("random.7z.001", "Release.7z.001"),
        ("random.part01.rar", "Release.part01.rar"),
        ("random.zip", "Release.zip"),
    ] {
        assert_eq!(
            client_archive_name(Path::new(physical), "random", "Release"),
            expected
        );
    }
}

#[test]
fn client_archive_name_can_publish_the_opaque_archive_stem() {
    for name in ["random.7z", "random.7z.001", "random.part01.rar"] {
        assert_eq!(
            client_archive_name(Path::new(name), "random", "random"),
            name
        );
    }
}

#[test]
fn client_archive_name_falls_back_for_unexpected_output() {
    assert_eq!(
        client_archive_name(Path::new("other.7z"), "random", "Release"),
        "other.7z"
    );
}

#[test]
fn portable_archive_stem_avoids_non_ascii_par2_metadata() {
    assert_eq!(portable_archive_stem("Release-01"), "Release-01");
    assert_eq!(portable_archive_stem("Árvore"), "archive");
    assert_eq!(portable_archive_stem(""), "archive");
}

#[test]
fn rar_volume_size_larger_than_content_falls_back_to_single_file() {
    if find_binary("rar").is_none() {
        eprintln!("skipping: rar CLI not installed");
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "pesto_compress_test_vol_fallback_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("input.bin");
    std::fs::write(&src, vec![0u8; 1_000_000]).unwrap();

    // Volume size (100m) far exceeds the 1MB input: rar decides one
    // volume is enough and writes plain `stem.rar`, no `.partNN` suffix.
    let result = compress(&[src], "stem", &dir, ArchiveFormat::Rar, None, Some("100m")).unwrap();

    assert!(result.path.exists());
    assert_eq!(result.path.file_name().unwrap(), "stem.rar");
    assert!(result.extra_paths.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn volume_size_accepts_digits_with_optional_unit() {
    for ok in ["500", "500m", "4g", "700k", "1T", "1b"] {
        assert!(
            validate_volume_size(ok).is_ok(),
            "expected `{ok}` to be accepted"
        );
    }
}

#[test]
fn volume_size_rejects_malformed_values() {
    for bad in ["", "mb", "500mb", "-5m", "5 m", "m5"] {
        assert!(
            validate_volume_size(bad).is_err(),
            "expected `{bad}` to be rejected"
        );
    }
}

#[test]
fn volume_size_rejects_zip_format() {
    let dir = std::env::temp_dir().join(format!("pesto_compress_test_{}", std::process::id()));
    let err = compress(&[], "stem", &dir, ArchiveFormat::Zip, None, Some("500m")).unwrap_err();
    assert!(err.to_string().contains("--compress-volume-size"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rar_volume_size_splits_archive_into_parts() {
    if find_binary("rar").is_none() {
        eprintln!("skipping: rar CLI not installed");
        return;
    }
    let dir = std::env::temp_dir().join(format!("pesto_compress_test_vol_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("input.bin");
    std::fs::write(&src, vec![0u8; 5_000_000]).unwrap();

    let result = compress(&[src], "stem", &dir, ArchiveFormat::Rar, None, Some("1m")).unwrap();

    assert!(result.path.exists());
    assert!(
        !result.extra_paths.is_empty(),
        "expected multiple volumes for a 5MB input split at 1MB"
    );
    for p in std::iter::once(&result.path).chain(result.extra_paths.iter()) {
        assert!(p.exists(), "volume `{}` missing on disk", p.display());
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sevenzip_volume_size_splits_archive_into_parts() {
    if find_binary("7z").is_none() {
        eprintln!("skipping: 7z CLI not installed");
        return;
    }
    let dir =
        std::env::temp_dir().join(format!("pesto_compress_test_7z_vol_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("input.bin");
    std::fs::write(&src, vec![0u8; 5_000_000]).unwrap();

    let result = compress(
        &[src],
        "stem",
        &dir,
        ArchiveFormat::SevenZip,
        None,
        Some("1m"),
    )
    .unwrap();

    assert!(result.path.exists());
    assert!(
        !result.extra_paths.is_empty(),
        "expected multiple volumes for a 5MB input split at 1MB"
    );
    for p in std::iter::once(&result.path).chain(result.extra_paths.iter()) {
        assert!(p.exists(), "volume `{}` missing on disk", p.display());
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn format_round_trips() {
    for (s, expected) in &[
        ("7z", ArchiveFormat::SevenZip),
        ("zip", ArchiveFormat::Zip),
        ("rar", ArchiveFormat::Rar),
    ] {
        assert_eq!(ArchiveFormat::parse(s), Some(*expected));
        assert_eq!(expected.extension(), *s);
    }
    assert_eq!(ArchiveFormat::parse("tar"), None);
}
#[test]
fn sevenzip_basename_only_contract() {
    if find_binary("7z").is_none() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("pesto_compress_7z_layout_{}", std::process::id()));
    let staging_dir = dir.join(".tmp/archive-source/my_release");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let src = staging_dir.join("input.bin");
    std::fs::write(&src, "test").unwrap();

    let result = compress(
        &[staging_dir],
        "stem",
        &dir,
        ArchiveFormat::SevenZip,
        None,
        None,
    )
    .unwrap();

    // Check archive contents using 7z l
    let output = std::process::Command::new("7z")
        .arg("l")
        .arg(&result.path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "7z list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains(".tmp/archive-source"));
    assert!(stdout.contains("my_release/input.bin") || stdout.contains(r"my_release\input.bin"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rar_basename_only_contract() {
    if find_binary("rar").is_none() {
        return;
    }
    let dir =
        std::env::temp_dir().join(format!("pesto_compress_rar_layout_{}", std::process::id()));
    let staging_dir = dir.join(".tmp/archive-source/my_release");
    std::fs::create_dir_all(&staging_dir).unwrap();
    let src = staging_dir.join("input.bin");
    std::fs::write(&src, "test").unwrap();

    let result = compress(&[staging_dir], "stem", &dir, ArchiveFormat::Rar, None, None).unwrap();

    let output = std::process::Command::new("rar")
        .arg("lb")
        .arg(&result.path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "RAR list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains(".tmp/archive-source"));
    assert!(
        stdout.contains("my_release/input.bin") || stdout.contains(r"my_release\input.bin"),
        "RAR must preserve the input basename as the archive root: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
