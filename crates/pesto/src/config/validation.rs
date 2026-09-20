use anyhow::{bail, Result};

/// Validate the raw `groups` entries resolved from CLI/config, before
/// [`crate::poster`]'s `pick_post_group` ever sees them.
///
/// Each entry is either a single newsgroup name, or several names joined
/// with `+` for a simultaneous cross-post (e.g. `"a.b.c+d.e.f"`). `,` is
/// accepted as a deprecated alias for `+` — before this was formalized, a
/// TOML array element with an embedded `,` (unlike `--groups` on the CLI,
/// which already splits on `,`) passed straight through to the `Newsgroups:`
/// header and happened to cross-post, purely as a parsing accident. Existing
/// configs relying on that keep working, with a nudge towards `+`, rather
/// than a hard break on upgrade.
pub fn validate_groups(groups: &[String]) -> Result<()> {
    for entry in groups {
        if entry.contains(',') {
            eprintln!(
                "warning: newsgroup entry `{entry}` uses ',' to cross-post — ',' is \
                 deprecated for this, use '+' instead (e.g. \"a.b.c+d.e.f\"); ',' will stop \
                 being treated as a cross-post separator in a future release"
            );
        }
        for part in entry.split(['+', ',']) {
            if part.trim().is_empty() {
                bail!("newsgroup entry `{entry}` has an empty name around '+'/','");
            }
        }
    }
    Ok(())
}

/// Check whether `s` consists strictly of valid XML 1.0 characters.
///
/// XML 1.0 Section 2.2 defines valid Char as:
/// `#x9 | #xA | #xD | [#x20-#xD7FF] | [#xE000-#xFFFD] | [#x10000-#x10FFFF]`
pub(crate) fn is_xml_representable(s: &str) -> bool {
    s.chars().all(|c| {
        matches!(
            c,
            '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}'
        )
    })
}

/// Validate encryption password configuration.
///
/// - Rejects empty password strings.
/// - Rejects XML-unrepresentable transport password characters before posting.
/// - Preserves valid whitespace (LF, tab, CR) and non-ASCII UTF-8 bytes verbatim without altering.
/// - If both `encrypt_password` and `nzb_password` are set, one silently wins
///   at the use site (`encrypt_password.or(nzb_password)`); there is no
///   archive-vs-encryption conflict rule.
pub(crate) fn validate_encryption(encrypt_password: Option<&str>) -> Result<()> {
    if let Some(pw) = encrypt_password {
        if pw.is_empty() {
            bail!("encryption password cannot be empty");
        }
        if !is_xml_representable(pw) {
            bail!("encryption password contains characters that cannot be represented in XML");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encryption_validation_empty_password_rejected() {
        assert!(validate_encryption(Some("")).is_err());
    }

    #[test]
    fn encryption_validation_whitespace_passwords_follow_std3_no_alteration() {
        // No silent alteration. A whitespace-only password is a valid
        // (if unusual) secret — it round-trips through XML verbatim, so it
        // is accepted as-is. Only the empty string and XML-unrepresentable
        // bytes are rejected.
        assert!(validate_encryption(Some("   ")).is_ok());
        assert!(validate_encryption(Some("\t\n")).is_ok());
    }

    #[test]
    fn encryption_validation_valid_password_accepted() {
        assert!(validate_encryption(Some("secret")).is_ok());
    }

    #[test]
    fn encryption_validation_none_accepted() {
        assert!(validate_encryption(None).is_ok());
    }

    #[test]
    fn single_group_is_valid() {
        assert!(validate_groups(&["alt.binaries.test".to_string()]).is_ok());
    }

    #[test]
    fn cross_post_target_is_valid() {
        assert!(validate_groups(&["alt.binaries.a+alt.binaries.b".to_string()]).is_ok());
    }

    #[test]
    fn pool_of_targets_is_valid() {
        let groups = vec![
            "alt.binaries.a+alt.binaries.b".to_string(),
            "alt.binaries.c".to_string(),
        ];
        assert!(validate_groups(&groups).is_ok());
    }

    #[test]
    fn comma_is_accepted_as_deprecated_alias() {
        // Warns on stderr (not asserted here, matching this crate's other
        // eprintln!-based warnings, e.g. walk.rs) but does not error.
        assert!(validate_groups(&["alt.binaries.a,alt.binaries.b".to_string()]).is_ok());
    }

    #[test]
    fn trailing_plus_is_rejected() {
        assert!(validate_groups(&["alt.binaries.a+".to_string()]).is_err());
    }

    #[test]
    fn leading_plus_is_rejected() {
        assert!(validate_groups(&["+alt.binaries.a".to_string()]).is_err());
    }

    #[test]
    fn double_plus_is_rejected() {
        assert!(validate_groups(&["alt.binaries.a++alt.binaries.b".to_string()]).is_err());
    }

    #[test]
    fn trailing_comma_is_rejected() {
        assert!(validate_groups(&["alt.binaries.a,".to_string()]).is_err());
    }

    #[test]
    fn whitespace_only_part_is_rejected() {
        assert!(validate_groups(&["alt.binaries.a+   ".to_string()]).is_err());
    }
}
