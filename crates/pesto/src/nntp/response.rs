//! NNTP status-response parsing and error classification.

use anyhow::{anyhow, Result};

/// A parsed NNTP status response: a three-digit code and the trailing text.
#[derive(Debug, Clone)]
pub struct Response {
    pub code: u16,
    pub text: String,
}

impl Response {
    /// Parse a single response line (`"code text\r\n"`).
    pub(super) fn parse(line: &str) -> Result<Response> {
        let line = line.trim_end_matches(['\r', '\n']);
        let code: u16 = line
            .get(..3)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| anyhow!("malformed NNTP response: {line:?}"))?;
        Ok(Response {
            code,
            text: line.get(4..).unwrap_or("").to_string(),
        })
    }
}

/// A likely cause for an NNTP server error, classified from its response
/// code and text. Mirrors `sabnzbd`'s `clues_login`/`clues_too_many`/
/// `clues_too_many_ip`/`clues_pay` (`sabnzbd/downloader.py`), which turn a
/// raw provider error into a specific, actionable diagnosis instead of just
/// forwarding the response text verbatim — the same "fail clearly, not with
/// a bare code" spirit this project's own design principles already call
/// for. Checked in the same priority `sabnzbd` uses in
/// `Downloader.finish_connect_nw`: a connection-limit clue is checked
/// before the generic login-failure clue, since a provider phrasing a
/// connection limit with a word like "access denied" would otherwise be
/// misclassified as a bad-credentials problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorHint {
    TooManyConnections,
    TooManyIpAddresses,
    LoginFailed,
    PaymentRequired,
}

impl ErrorHint {
    /// A short, actionable message for this hint. Never includes any part
    /// of the server's own response text, so it is safe to attach even to
    /// errors from commands that may carry credentials (`AUTHINFO PASS`).
    pub fn message(self) -> &'static str {
        match self {
            ErrorHint::TooManyConnections => {
                "the server is rejecting new connections — lower `connections` for it in your config"
            }
            ErrorHint::TooManyIpAddresses => {
                "the server reports logins from too many different IP addresses for this account — it may be shared or already in use elsewhere"
            }
            ErrorHint::LoginFailed => {
                "check the username/password for this server in your config"
            }
            ErrorHint::PaymentRequired => {
                "this account may need renewal or has exceeded its quota with this provider"
            }
        }
    }
}

/// Classify an NNTP error response into a likely cause. `text` is only
/// pattern-matched here — never echoed back by [`ErrorHint::message`] — so
/// this is safe to call even on a response that might carry credentials.
pub fn classify_error(code: u16, text: &str) -> Option<ErrorHint> {
    let lower = text.to_lowercase();
    let has_any = |clues: &[&str]| clues.iter().any(|c| lower.contains(c));

    if matches!(code, 502 | 400 | 481 | 482)
        && has_any(&["exceed", "connections", "too many", "threads", "limit"])
        && !lower.contains("download")
        && !lower.contains("byte")
    {
        return Some(ErrorHint::TooManyConnections);
    }
    if matches!(code, 502 | 481 | 482) && has_any(&["simultaneous ip", "multiple ip"]) {
        return Some(ErrorHint::TooManyIpAddresses);
    }
    if matches!(code, 452 | 481 | 482 | 381)
        || (matches!(code, 500 | 502)
            && has_any(&["username", "password", "invalid", "authen", "access denied"]))
    {
        return Some(ErrorHint::LoginFailed);
    }
    // Note: 482 is deliberately absent here (unlike the `TooManyConnections`/
    // `LoginFailed` code sets above) — it's already caught unconditionally by
    // `LoginFailed`'s code-only branch, so by the time a response reaches
    // this check its code can only ever be 502. "exceeded" is deliberately
    // *not* a clue here (unlike `sabnzbd`'s `clues_pay`, which included it):
    // it collides with byte/download-quota wording ("download limit
    // exceeded") that has nothing to do with account payment status, and
    // `sabnzbd` gets away with the overlap only because `clues_pay` there
    // picks a retry penalty, never a distinct user-facing message the way
    // `ErrorHint::PaymentRequired` is used here.
    if code == 502 && has_any(&["credits", "paym", "expired"]) {
        return Some(ErrorHint::PaymentRequired);
    }
    None
}

/// Appends a [`classify_error`] hint to `base`, if one applies.
pub(super) fn with_hint(code: u16, text: &str, base: String) -> String {
    match classify_error(code, text) {
        Some(hint) => format!("{base} — {}", hint.message()),
        None => base,
    }
}
