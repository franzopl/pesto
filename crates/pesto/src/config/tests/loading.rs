use crate::config::*;
use std::io::Write;
use tempfile::NamedTempFile;

#[test]
fn file_config_load_from_disk() {
    let mut f = NamedTempFile::new().unwrap();
    write!(
        f,
        "[server]\nhost = \"disk-host\"\n[posting]\ngroups = [\"a\"]\n"
    )
    .unwrap();
    let loaded = FileConfig::load(f.path()).unwrap();
    assert_eq!(loaded.server.host.as_deref(), Some("disk-host"));
}

#[test]
fn file_config_load_missing_file_errors() {
    let err = FileConfig::load(std::path::Path::new("/no/such/file.toml")).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("reading config file"));
}

#[test]
fn misplaced_temp_dir_hints_at_compression_section() {
    let mut f = NamedTempFile::new().unwrap();
    write!(
        f,
        "[server]\nhost = \"h\"\n[output]\ntemp_dir = \"/tmp/x\"\n"
    )
    .unwrap();
    let err = FileConfig::load(f.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("`temp_dir` belongs under [compression]"),
        "unexpected message: {msg}"
    );
}

#[test]
fn misplaced_par2_temp_dir_hints_at_posting_section() {
    let mut f = NamedTempFile::new().unwrap();
    write!(
        f,
        "[server]\nhost = \"h\"\n[compression]\npar2_temp_dir = \"/tmp/x\"\n"
    )
    .unwrap();
    let err = FileConfig::load(f.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("`par2_temp_dir` belongs under [posting]"),
        "unexpected message: {msg}"
    );
}
