//! NNTP wire helpers: dot-stuffing and posted-article detection.

/// Decide whether a `441` POST rejection actually means the article is already
/// present on the server, in which case the POST has effectively succeeded.
///
/// When a connection drops after the server accepted an article but before we
/// read its `240`, a retry re-sends the same Message-ID. The server then
/// answers `441` wrapping a `435 Already exists in history` (RFC 3977 §6.2.2:
/// code 435 = "article not wanted; already have it"). Some servers instead
/// phrase the same rejection as a non-unique Message-ID (e.g. "Message-ID is
/// not unique") without the `435` code or that exact wording. Treating either
/// phrasing as success avoids a pointless retry storm over segments that are
/// already posted.
pub(super) fn already_exists(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("already exists") || lower.contains("435") || lower.contains("not unique")
}

/// Extract a `<message-id>` echoed at the start of a successful POST
/// response's text, if present.
///
/// RFC 3977 §6.3.1.3 does not require a server to echo a Message-ID in its
/// `240` response, but some do — and at least some of those substitute a
/// *different* ID than the one the client sent, at their own discretion
/// (e.g. deduplication or canonicalization applied at accept time). A client
/// that keeps tracking the ID it generated in that case will never find the
/// article again via `STAT`, because that ID was never the one actually
/// used to store it — the server's response is the only place this shows
/// up. `nyuu` has handled this since 2016 (its `RE_POST` matcher); this
/// mirrors that behavior so `pesto` trusts whichever ID the server says it
/// used.
pub(super) fn extract_returned_message_id(text: &str) -> Option<String> {
    let text = text.trim_start();
    if !text.starts_with('<') {
        return None;
    }
    let end = text.find('>')?;
    Some(text[..=end].to_string())
}

/// Whether `line` is the dot-terminated block's end-of-data marker: a line
/// that is exactly `.`, with either CRLF or bare LF termination.
pub(super) fn is_dot_terminator(line: &[u8]) -> bool {
    let trimmed = line
        .strip_suffix(b"\r\n")
        .or_else(|| line.strip_suffix(b"\n"))
        .unwrap_or(line);
    trimmed == b"."
}

/// True when any line of a yEnc body starts with `.`.
///
/// The poster relies on the encoder never producing that (see
/// [`post_parts_inner`] / [`NntpConnection::enqueue_post`]); this is the
/// predicate those `debug_assert!`s use.
pub(crate) fn yenc_body_has_leading_dot(body: &[u8]) -> bool {
    body.starts_with(b".") || body.windows(3).any(|w| w == b"\r\n.")
}

/// Apply NNTP dot-stuffing: any line that begins with `.` gets an extra `.`
/// prepended, so it cannot be mistaken for the end-of-data marker.
pub(super) fn dot_stuff(input: &[u8], out: &mut Vec<u8>) {
    let mut at_line_start = true;
    for &b in input {
        if at_line_start && b == b'.' {
            out.push(b'.');
        }
        out.push(b);
        at_line_start = b == b'\n';
    }
}
