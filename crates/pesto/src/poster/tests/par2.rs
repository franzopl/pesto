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
