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

#[cfg(test)]
mod tests {
    use super::*;

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
