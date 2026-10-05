use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

/// Create a unique temp directory for one test.
fn temp_dir() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pesto_walk_{}_{n}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, b"x").unwrap();
}

#[test]
fn plain_files_keep_their_base_name() {
    let dir = temp_dir();
    let a = dir.join("a.bin");
    let b = dir.join("b.bin");
    touch(&a);
    touch(&b);

    let out = expand_inputs(&[b.clone(), a.clone()]).unwrap();
    assert_eq!(out.len(), 2);
    // Sorted by name regardless of argument order.
    assert_eq!(out[0].name, "a.bin");
    assert_eq!(out[1].name, "b.bin");

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn directory_is_walked_recursively_with_relative_names() {
    let dir = temp_dir();
    let season = dir.join("Season 01");
    touch(&season.join("ep01.mkv"));
    touch(&season.join("ep02.mkv"));
    touch(&season.join("extras/behind.mkv"));

    let out = expand_inputs(std::slice::from_ref(&season)).unwrap();
    let names: Vec<&str> = out.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Season 01/ep01.mkv",
            "Season 01/ep02.mkv",
            "Season 01/extras/behind.mkv"
        ]
    );

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn hidden_entries_are_included() {
    let dir = temp_dir();
    let root = dir.join("show");
    touch(&root.join("ep01.mkv"));
    touch(&root.join(".hidden.nfo"));
    touch(&root.join(".meta/info.txt"));

    let out = expand_inputs(std::slice::from_ref(&root)).unwrap();
    let names: Vec<&str> = out.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        ["show/.hidden.nfo", "show/.meta/info.txt", "show/ep01.mkv"]
    );

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn default_metadata_patterns_are_excluded_at_every_depth() {
    let dir = temp_dir();
    let names = [
        ".DS_Store",
        "._movie.mkv",
        ".fuse_hidden123",
        ".Spotlight-V100",
        ".Trashes",
        ".fseventsd",
        "Thumbs.db",
        "ehthumbs.db",
        "desktop.ini",
        "@eaDir",
    ];
    for name in names {
        touch(&dir.join(name));
        touch(&dir.join("nested").join(name));
    }
    touch(&dir.join("movie.mkv"));
    touch(&dir.join(".hidden.mkv"));
    touch(&dir.join("movie.nfo"));
    touch(&dir.join("movie.srt"));
    let out = expand_inputs(std::slice::from_ref(&dir)).unwrap();
    assert_eq!(out.len(), 4);
    assert!(out.iter().any(|f| f.path.ends_with(".hidden.mkv")));
    // AppleDouble has a media extension too; extension allowlists cannot
    // distinguish it from content, so exclusions must run first.
    assert_eq!(Path::new("._movie.mkv").extension().unwrap(), "mkv");
    assert_eq!(Path::new(".hidden.mkv").extension().unwrap(), "mkv");
    let all =
        expand_inputs_with_options(std::slice::from_ref(&dir), &["*.mkv".into()], true).unwrap();
    assert_eq!(all.len(), names.len() * 2 + 4);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn excluded_directories_prune_the_whole_subtree() {
    let dir = temp_dir();
    for name in [
        ".Spotlight-V100",
        ".Trashes",
        ".fseventsd",
        "@eaDir",
        "._cache",
        ".fuse_hidden123",
    ] {
        touch(&dir.join(name).join("content.mkv"));
        touch(&dir.join("nested").join(name).join("content.mkv"));
    }
    touch(&dir.join("movie.mkv"));
    assert_eq!(expand_inputs(std::slice::from_ref(&dir)).unwrap().len(), 1);
    // An explicitly supplied directory remains a root even if its name
    // matches; only discovered entries are excluded.
    assert_eq!(expand_inputs(&[dir.join("@eaDir")]).unwrap().len(), 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn explicit_files_bypass_default_and_custom_exclusions() {
    let dir = temp_dir();
    let file = dir.join("._movie.mkv");
    touch(&file);
    assert_eq!(
        expand_inputs_with_options(&[file], &["*".into()], false)
            .unwrap()
            .len(),
        1
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn exclusions_use_case_sensitive_relative_globs() {
    let filters = Exclusions::new(
        &[
            "*.tmp".into(),
            "sample?.[mn]kv".into(),
            "extras/*.mkv".into(),
            "**/cache".into(),
            "*/.custom".into(),
        ],
        false,
    )
    .unwrap();
    for (name, path) in [
        ("file.tmp", "deep/file.tmp"),
        ("sample1.mkv", "sample1.mkv"),
        ("sample2.nkv", "deep/sample2.nkv"),
        ("ep.mkv", "extras/ep.mkv"),
        ("cache", "a/b/cache"),
        (".custom", "nested/.custom"),
    ] {
        assert!(filters.matches(name, path), "{path}");
    }
    for (name, path) in [
        ("file.TMP", "file.TMP"),
        ("sample12.mkv", "sample12.mkv"),
        ("ep.mkv", "extras/deep/ep.mkv"),
        (".hidden.mkv", ".hidden.mkv"),
        (".ds_store", ".ds_store"),
    ] {
        assert!(!filters.matches(name, path), "{path}");
    }
    assert!(!Exclusions::new(&["/absolute/*.mkv".into()], false)
        .unwrap()
        .matches("ep.mkv", "absolute/ep.mkv"));
    assert!(Exclusions::new(&["[".into()], false).is_err());
    assert!(!Exclusions::new(&["[".into()], true)
        .unwrap()
        .matches(".DS_Store", ".DS_Store"));
}

#[test]
fn custom_relative_globs_work_for_absolute_directory_arguments() {
    let dir = temp_dir();
    touch(&dir.join("extras/sample1.mkv"));
    touch(&dir.join("extras/ep2.mkv"));
    touch(&dir.join("extras/ep10.mkv"));
    touch(&dir.join("cache/keep.mkv"));
    let out = expand_inputs_with_options(
        std::slice::from_ref(&dir),
        &["extras/sample?.mkv".into(), "cache".into()],
        false,
    )
    .unwrap();
    assert_eq!(out.len(), 2);
    assert!(out[0].path.ends_with("extras/ep2.mkv"));
    assert!(out[1].path.ends_with("extras/ep10.mkv"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn directory_containing_only_excluded_entries_is_rejected() {
    let dir = temp_dir();
    touch(&dir.join(".DS_Store"));
    assert!(expand_inputs(std::slice::from_ref(&dir))
        .unwrap_err()
        .to_string()
        .contains("no files to post"));
    touch(&dir.join("movie.mkv"));
    assert!(expand_inputs_with_options(std::slice::from_ref(&dir), &["*".into()], false).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn empty_directory_is_rejected() {
    let dir = temp_dir();
    let root = dir.join("empty");
    fs::create_dir_all(&root).unwrap();

    assert!(expand_inputs(std::slice::from_ref(&root)).is_err());

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn missing_path_is_an_error() {
    let dir = temp_dir();
    let missing = dir.join("nope.bin");
    assert!(expand_inputs(&[missing]).is_err());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn colliding_names_are_rejected() {
    let dir = temp_dir();
    let a = dir.join("one/movie.mkv");
    let b = dir.join("two/movie.mkv");
    touch(&a);
    touch(&b);
    // Two files given directly, both with base name `movie.mkv`.
    assert!(expand_inputs(&[a, b]).is_err());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn symlinks_inside_directory_are_skipped() {
    let dir = temp_dir();
    let root = dir.join("show");
    touch(&root.join("ep01.mkv"));

    // Create a symlink inside the directory; it should be skipped.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("ep01.mkv"), root.join("link.mkv")).unwrap();
    }
    #[cfg(not(unix))]
    {
        // On non-Unix, skip the symlink part of this test.
        fs::remove_dir_all(&dir).unwrap();
        return;
    }

    let out = expand_inputs(std::slice::from_ref(&root)).unwrap();
    // Only the real file; the symlink is silently skipped.
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].name, "show/ep01.mkv");

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn published_names_replace_control_characters() {
    assert_eq!(
        sanitize_published_name("ok\r\nfile.bin").unwrap(),
        "ok__file.bin"
    );
    assert_eq!(sanitize_published_name("a\0b").unwrap(), "a_b");
    assert!(sanitize_published_name("").is_err());
}

#[test]
fn empty_paths_list_is_an_error() {
    assert!(expand_inputs(&[]).is_err());
}

#[test]
fn output_is_sorted_by_name() {
    let dir = temp_dir();
    touch(&dir.join("z.bin"));
    touch(&dir.join("a.bin"));
    touch(&dir.join("m.bin"));

    let out = expand_inputs(&[dir.join("z.bin"), dir.join("a.bin"), dir.join("m.bin")]).unwrap();
    let names: Vec<&str> = out.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["a.bin", "m.bin", "z.bin"]);

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn output_is_sorted_naturally_not_lexicographically() {
    let dir = temp_dir();
    for n in ["1", "2", "10", "12"] {
        touch(&dir.join(format!("ep{n}.mkv")));
    }

    let out = expand_inputs(std::slice::from_ref(&dir)).unwrap();
    let names: Vec<&str> = out.iter().map(|f| f.name.as_str()).collect();
    let root = dir.file_name().unwrap().to_string_lossy();
    assert_eq!(
        names,
        [
            format!("{root}/ep1.mkv"),
            format!("{root}/ep2.mkv"),
            format!("{root}/ep10.mkv"),
            format!("{root}/ep12.mkv"),
        ]
    );

    fs::remove_dir_all(&dir).unwrap();
}

// ── natural_cmp ──────────────────────────────────────────────────────────

fn natural_sorted(names: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    v.sort_by(|a, b| natural_cmp(a, b));
    v
}

#[test]
fn natural_cmp_orders_unpadded_volume_numbers_numerically() {
    assert_eq!(
        natural_sorted(&[
            "release.part10.rar",
            "release.part2.rar",
            "release.part1.rar",
            "release.part12.rar",
        ]),
        vec![
            "release.part1.rar",
            "release.part2.rar",
            "release.part10.rar",
            "release.part12.rar",
        ]
    );
}

#[test]
fn natural_cmp_orders_padded_and_7z_volume_numbers() {
    assert_eq!(
        natural_sorted(&["a.7z.010", "a.7z.002", "a.7z.001"]),
        vec!["a.7z.001", "a.7z.002", "a.7z.010"]
    );
    assert_eq!(
        natural_sorted(&["r.part003.rar", "r.part001.rar", "r.part002.rar"]),
        vec!["r.part001.rar", "r.part002.rar", "r.part003.rar"]
    );
}

#[test]
fn natural_cmp_falls_back_to_byte_order_outside_digit_runs() {
    assert_eq!(
        natural_sorted(&["s01/ep10.mkv", "s01/ep2.mkv", "s01/ep1.mkv", "s01/a.nfo"]),
        vec!["s01/a.nfo", "s01/ep1.mkv", "s01/ep2.mkv", "s01/ep10.mkv"]
    );
}

#[test]
fn natural_cmp_is_a_total_order_across_equal_valued_padding() {
    // Same numeric value, different padding: whichever way the tie breaks,
    // it must break — two distinct names comparing Equal would make the
    // sort order depend on directory-listing order, and with it the PAR2
    // set and every wire name derived from the file's position.
    assert_eq!(natural_cmp("p1.rar", "p1.rar"), std::cmp::Ordering::Equal);
    assert_ne!(
        natural_cmp("p01.rar", "p1.rar"),
        std::cmp::Ordering::Equal,
        "distinct names must never compare Equal"
    );
    assert_eq!(
        natural_cmp("p01.rar", "p1.rar").reverse(),
        natural_cmp("p1.rar", "p01.rar"),
        "the comparator must be antisymmetric"
    );
}

#[test]
fn natural_cmp_matches_the_comparator_each_orders_entries_with() {
    // `top_level_entries` (`--each`/`--season`) sorts with this same
    // function; if the two ever diverged, a season's entry order and a
    // release's internal file order would disagree.
    for (a, b) in [
        ("ep1.mkv", "ep10.mkv"),
        ("B.txt", "b.txt"),
        ("Show/ep2.bin", "Show/ep11.bin"),
    ] {
        assert_eq!(natural_cmp(a, b), lexical_sort::natural_lexical_cmp(a, b));
    }
}

#[test]
fn split_discovery_keeps_original_root_for_path_globs() {
    let dir = temp_dir();
    touch(&dir.join("extras/sample.mkv"));
    touch(&dir.join("extras/keep.mp4"));
    touch(&dir.join("extras/nested/sample.mkv"));
    let globs = vec!["extras/*.mkv".into()];
    let files = expand_inputs_from_root(&[dir.join("extras")], &dir, &globs, false).unwrap();
    assert_eq!(files.len(), 2);
    assert!(files
        .iter()
        .all(|f| f.path != dir.join("extras/sample.mkv")));
    assert!(
        expand_inputs_from_root(&[dir.join("extras/sample.mkv")], &dir, &globs, false).is_err()
    );
    assert_eq!(
        expand_inputs_with_options(&[dir.join("extras/sample.mkv")], &globs, false)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        expand_inputs_from_root(&[dir.join("extras")], &dir, &globs, true)
            .unwrap()
            .len(),
        3
    );
    fs::remove_dir_all(dir).unwrap();
}
