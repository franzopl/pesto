use crate::config::{parse_memory_limit_spec, parse_upload_rate};

#[test]
fn parse_rate_bare_bytes() {
    assert_eq!(parse_upload_rate("1024").unwrap(), 1024);
}

#[test]
fn parse_rate_kib() {
    assert_eq!(parse_upload_rate("10 KiB/s").unwrap(), 10 * 1024);
}

#[test]
fn parse_rate_mib() {
    assert_eq!(parse_upload_rate("50 MiB/s").unwrap(), 50 * 1024 * 1024);
}

#[test]
fn parse_rate_mb_case_insensitive() {
    assert_eq!(parse_upload_rate("2 MB/s").unwrap(), 2 * 1024 * 1024);
}

#[test]
fn parse_rate_gib() {
    assert_eq!(parse_upload_rate("1 GiB/s").unwrap(), 1024 * 1024 * 1024);
}

#[test]
fn parse_rate_unknown_unit_errors() {
    assert!(parse_upload_rate("10 TiB/s").is_err());
}

#[test]
fn parse_rate_not_a_number_errors() {
    assert!(parse_upload_rate("fast").is_err());
}

#[test]
fn memory_limit_spec_auto_and_empty_mean_no_explicit_value() {
    assert_eq!(parse_memory_limit_spec("auto").unwrap(), None);
    assert_eq!(parse_memory_limit_spec("Auto").unwrap(), None);
    assert_eq!(parse_memory_limit_spec("").unwrap(), None);
    assert_eq!(parse_memory_limit_spec("  ").unwrap(), None);
}

#[test]
fn memory_limit_spec_absolute_size_delegates_to_parse_upload_rate() {
    assert_eq!(
        parse_memory_limit_spec("8 GiB").unwrap(),
        Some(8 * 1024 * 1024 * 1024)
    );
}

#[test]
fn memory_limit_spec_percentage_resolves_against_host_ram() {
    let resolved = parse_memory_limit_spec("50%").unwrap().unwrap();
    // Can't assert an exact value (depends on the test host's RAM), but it
    // must be a real, non-trivial fraction of it.
    assert!(resolved > 0);
}

#[test]
fn memory_limit_spec_percentage_out_of_range_errors() {
    assert!(parse_memory_limit_spec("150%").is_err());
    assert!(parse_memory_limit_spec("-10%").is_err());
}

#[test]
fn memory_limit_spec_invalid_percentage_number_errors() {
    assert!(parse_memory_limit_spec("many%").is_err());
}
