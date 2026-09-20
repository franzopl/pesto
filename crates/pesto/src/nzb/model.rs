//! Shared NZB model: head metadata, external-reference IDs and escaping.

use crate::poster::PostedSegment;

/// NZB `<head>` metadata fields emitted as `<meta type="...">` elements.
///
/// All fields are optional. NZBGet and SABnzbd recognise `title`, `password`
/// and `category` natively; other values are ignored by those clients but
/// kept in the XML for informational use.
#[derive(Debug, Default, Clone)]
pub struct NzbMeta {
    /// Friendly display name for the download (`<meta type="title">`,
    /// SABnzbd's documented meta type for a human-readable NZB name).
    pub name: Option<String>,
    /// Extraction password (`<meta type="password">`).
    /// Set this from the archive password when `--nzb-password` is absent.
    pub password: Option<String>,
    /// Indexer / downloader category (`<meta type="category">`).
    pub category: Option<String>,
    /// TMDb reference (`<meta type="tmdbid">`), formatted as `movie/<id>` or
    /// `tv/<id>` — see [`parse_tmdb_ref`].
    pub tmdb_id: Option<String>,
    /// IMDb ID (`<meta type="imdbid">`), e.g. `tt1234567` — see [`parse_imdb_ref`].
    pub imdb_id: Option<String>,
    /// TheTVDB numeric ID (`<meta type="tvdbid">`), movie/series kind
    /// stripped — see [`parse_tvdb_ref`].
    pub tvdb_id: Option<String>,
    /// MyAnimeList ID (`<meta type="malid">`) — see [`parse_mal_ref`].
    pub mal_id: Option<String>,
    /// Arbitrary tags emitted as multiple `<meta type="tag">` elements.
    pub tags: Vec<String>,
}

/// Media type of a [`parse_tmdb_ref`] result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmdbKind {
    Movie,
    Tv,
}

impl TmdbKind {
    fn as_str(self) -> &'static str {
        match self {
            TmdbKind::Movie => "movie",
            TmdbKind::Tv => "tv",
        }
    }

    /// `.nzb` category to fall back to when the user hasn't set one explicitly.
    pub fn default_category(self) -> &'static str {
        match self {
            TmdbKind::Movie => "movies",
            TmdbKind::Tv => "tv",
        }
    }
}

/// Parse a `--tmdb` value into its media kind and numeric ID.
///
/// Accepts `movie/<id>` or `tv/<id>`, matching TMDb's own `/movie/<id>` and
/// `/tv/<id>` URL scheme. `:` is also accepted as the separator
/// (`movie:<id>`), since some indexer tools use that convention instead.
pub fn parse_tmdb_ref(s: &str) -> Result<(TmdbKind, String), String> {
    let (kind_str, id) = s
        .split_once(['/', ':'])
        .ok_or_else(|| format!("expected `movie/<id>` or `tv/<id>`, got `{s}`"))?;
    let kind = match kind_str.to_ascii_lowercase().as_str() {
        "movie" => TmdbKind::Movie,
        "tv" => TmdbKind::Tv,
        other => {
            return Err(format!(
                "unknown TMDb media type `{other}` (expected `movie` or `tv`)"
            ))
        }
    };
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("expected a numeric TMDb ID, got `{id}`"));
    }
    Ok((kind, id.to_owned()))
}

/// Normalize a parsed [`parse_tmdb_ref`] result into the value stored in
/// [`NzbMeta::tmdb_id`], e.g. `("movie", "12345")` -> `"movie/12345"`.
pub fn format_tmdb_ref(kind: TmdbKind, id: &str) -> String {
    format!("{}/{id}", kind.as_str())
}

/// Parse a `--imdb-id` value into its normalized form.
///
/// Accepts an optional `tt` prefix (case-insensitive) followed by digits;
/// the `tt` prefix is added when missing. IMDb IDs are zero-padded to at
/// least 7 digits (e.g. `133093` and `tt0133093` both normalize to
/// `tt0133093`); longer IDs are kept as-is.
pub fn parse_imdb_ref(s: &str) -> Result<String, String> {
    let trimmed = s.trim();
    let digits = trimmed
        .strip_prefix("tt")
        .or_else(|| trimmed.strip_prefix("TT"))
        .or_else(|| trimmed.strip_prefix("Tt"))
        .or_else(|| trimmed.strip_prefix("tT"))
        .unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("expected an IMDb ID like `tt1234567`, got `{s}`"));
    }
    Ok(format!("tt{digits:0>7}"))
}

/// Media type of a [`parse_tvdb_ref`] result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TvdbKind {
    Movie,
    Series,
}

impl TvdbKind {
    /// URL segment used by TheTVDB's own dereferrer
    /// (`/dereferrer/movie/<id>` or `/dereferrer/series/<id>`).
    pub fn as_str(self) -> &'static str {
        match self {
            TvdbKind::Movie => "movie",
            TvdbKind::Series => "series",
        }
    }

    /// `.nzb` category to fall back to when the user hasn't set one explicitly.
    pub fn default_category(self) -> &'static str {
        match self {
            TvdbKind::Movie => "movies",
            TvdbKind::Series => "tv",
        }
    }
}

/// Parse a `--tvdb-id` value into its media kind and numeric ID.
///
/// Accepts `movie/<id>` or `series/<id>` (`tv` also accepted as an alias for
/// `series`, matching the `--tmdb` convention), mirroring TheTVDB's own
/// `/movies/<slug>` and `/series/<slug>` split. `:` is also accepted as the
/// separator (`movie:<id>`), same as `--tmdb`.
///
/// A bare numeric ID with no `/` or `:` (e.g. `81189`) is still accepted for
/// backwards compatibility and defaults to `series` — TheTVDB's original,
/// and still by far most common, content type — so existing configs and
/// scripts keep working unchanged.
pub fn parse_tvdb_ref(s: &str) -> Result<(TvdbKind, String), String> {
    match s.split_once(['/', ':']) {
        Some((kind_str, id)) => {
            let kind = match kind_str.to_ascii_lowercase().as_str() {
                "movie" => TvdbKind::Movie,
                "series" | "tv" => TvdbKind::Series,
                other => {
                    return Err(format!(
                        "unknown TVDB media type `{other}` (expected `movie` or `series`)"
                    ))
                }
            };
            let id = parse_numeric_ref(id, "TVDB")?;
            Ok((kind, id))
        }
        None => {
            let id = parse_numeric_ref(s, "TVDB")?;
            Ok((TvdbKind::Series, id))
        }
    }
}

/// Parse a `--mal-id` value into its normalized form: a plain numeric string.
pub fn parse_mal_ref(s: &str) -> Result<String, String> {
    parse_numeric_ref(s, "MAL")
}

fn parse_numeric_ref(s: &str, label: &str) -> Result<String, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("expected a numeric {label} ID, got `{s}`"));
    }
    Ok(trimmed.to_owned())
}

/// The contents of a parsed `.nzb` file.
pub struct ParsedNzb {
    /// `From` header found in the first `<file>` element.
    pub poster: String,
    /// Newsgroups listed in `<groups>` (deduplicated, first file wins).
    pub groups: Vec<String>,
    /// All segments, sorted by `(file_name, part)`.
    pub segments: Vec<PostedSegment>,
    /// `<head>` metadata (`name`, `password`, `category`, `tags`).
    pub meta: NzbMeta,
}

/// Escape the five XML predefined entities.
pub(super) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 forbids C0 controls other than tab / LF / CR.
            c if c.is_control() && !matches!(c, '\t' | '\n' | '\r') => {}
            _ => out.push(c),
        }
    }
    out
}
