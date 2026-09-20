//! `.nzb` generation, parsing and the shared metadata model.

mod model;
mod reader;
mod writer;

use model::*;
#[cfg(test)]
use reader::*;

#[cfg(test)]
mod tests;

pub use model::{
    format_tmdb_ref, parse_imdb_ref, parse_mal_ref, parse_tmdb_ref, parse_tvdb_ref, NzbMeta,
    ParsedNzb, TmdbKind, TvdbKind,
};
pub use reader::parse;
pub use writer::{generate, wire_subject, wire_subjects};
