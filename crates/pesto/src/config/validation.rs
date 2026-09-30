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

/// Validate encryption password configuration.
///
/// - Rejects empty password strings.
/// - Rejects conflicting passwords if both `encrypt_password` and `nzb_password` are specified and differ.
pub fn validate_encryption(
    encrypt_password: Option<&str>,
    nzb_password: Option<&str>,
) -> Result<()> {
    if let Some(pw) = encrypt_password {
        if pw.is_empty() {
            bail!("encryption password cannot be empty");
        }
        if let Some(nzb_pw) = nzb_password {
            if !nzb_pw.is_empty() && nzb_pw != pw {
                bail!("conflicting passwords: --encrypt-password and --nzb-password cannot differ");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encryption_validation_empty_password_rejected() {
        assert!(validate_encryption(Some(""), None).is_err());
    }

    #[test]
    fn encryption_validation_valid_password_accepted() {
        assert!(validate_encryption(Some("secret"), None).is_ok());
        assert!(validate_encryption(Some("secret"), Some("secret")).is_ok());
    }

    #[test]
    fn encryption_validation_conflicting_passwords_rejected() {
        assert!(validate_encryption(Some("secret"), Some("other")).is_err());
    }

    #[test]
    fn encryption_validation_none_accepted() {
        assert!(validate_encryption(None, None).is_ok());
        assert!(validate_encryption(None, Some("nzbpass")).is_ok());
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
