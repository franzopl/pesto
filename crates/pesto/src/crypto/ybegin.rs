//! Shared `=ybegin` `name=` token-boundary lookup (Body Encryption
//! Standard v1.2 §2.1, settled Q-B): the keywords are the space-separated
//! tokens *before* `name=`; the `name=` value is the remainder of the line.
//! A keyword value that itself contains the substring `name=` (e.g.
//! `line=name=128`) must not be mistaken for the real name — the token
//! boundary is the start of the line or a space/tab immediately before
//! `name=`.

/// Byte index of the `name=` token at its Q-B token boundary in `line`, if
/// any. Keywords are the space-separated tokens before `name=`, so the real
/// `name=` is the start of the *last* token: scan tokens from the left and
/// return the first token that begins with `name=`. A `name=` embedded
/// inside another token's value (e.g. the token `part=name=1`) is not at a
/// token start and is skipped, while a filename containing ` name=` still
/// resolves to the first `name=` token (its value is the remainder).
pub(crate) fn name_token_boundary(line: &[u8]) -> Option<usize> {
    // Tokenize on space/tab, skipping empty tokens from repeated separators.
    for token in line.split(|&b| b == b' ' || b == b'\t') {
        // `split` yields slices; recover the offset via pointer arithmetic.
        let start = token.as_ptr() as usize - line.as_ptr() as usize;
        if token.starts_with(b"name=") {
            return Some(start);
        }
        if token.is_empty() {
            continue;
        }
    }
    None
}

/// Raw bytes of the `name=` value on a restored `=ybegin` line: the value
/// runs from just after the token-boundary `name=` to the end of the first
/// line (Body Std v1.2 §2.1), so no byte is decoded.
pub(crate) fn ybegin_raw_name(restored: &[u8]) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context;
    let end = restored
        .iter()
        .position(|&b| b == b'\n')
        .context("restored article has no =ybegin line terminator")?;
    let line = restored[..end]
        .strip_suffix(b"\r")
        .unwrap_or(&restored[..end]);
    let pos = name_token_boundary(line)
        .context("MISSING_AAD_INPUT: =ybegin line has no name= parameter")?;
    Ok(line[pos + 5..].to_vec())
}
