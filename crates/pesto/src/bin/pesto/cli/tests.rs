use super::*;

#[test]
fn exclusion_flags_produce_optional_overrides() {
    let cli = Cli::try_parse_from(["pesto", "movie.mkv"]).unwrap();
    let overrides = cli.overrides();
    assert!(overrides.ext.is_none());
    assert!(overrides.exclude.is_none());
    assert!(overrides.no_exclude.is_none());

    let cli = Cli::try_parse_from([
        "pesto",
        "--exclude",
        "*.tmp",
        "--exclude",
        "sample?.mkv",
        "--no-exclude",
        "--ext",
        "mkv,srt",
        "movie.mkv",
    ])
    .unwrap();
    let overrides = cli.overrides();
    assert_eq!(overrides.exclude.unwrap(), ["*.tmp", "sample?.mkv"]);
    assert_eq!(overrides.no_exclude, Some(true));
    assert_eq!(overrides.ext.unwrap(), ["mkv", "srt"]);
}
