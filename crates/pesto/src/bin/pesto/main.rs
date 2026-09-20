//! `pesto` — fast, lean Usenet poster.
//!
//! Parses the CLI, resolves the configuration, posts the given files to Usenet
//! and writes an `.nzb` file describing the result.

use anyhow::{Context, Result};
use pesto::config::{Config, ObfuscateMode};
use tracing::info;

mod batch;
mod cleanup;
mod cli;
mod command;
mod hooks;
mod merge;
mod output;
mod season;
mod summary;
mod upload;
mod watch;

use hooks::{run_all_hooks, HookEnv};
use upload::run_single_upload;

/// Tracks this process's exact live-heap byte count (see
/// [`pesto::memory::alloc`]), for comparison against `VmSize`/`RLIMIT_AS` in
/// `--memory-report`. Declared here — in the binary, not the `pesto` library
/// — because `#[global_allocator]` is a whole-binary choice; `upapasta`,
/// `penne` and `sugo` link `pesto` as a library and are unaffected by it.
#[global_allocator]
static ALLOC: pesto::memory::alloc::CountingAlloc = pesto::memory::alloc::CountingAlloc::new();

/// Build the `IMDb:`/`TMDb:`/`TVDB:`/`MAL:` header block prepended to a
/// generated `.nfo` when any of `--tmdb`, `--imdb-id`, `--tvdb-id` or
/// `--mal-id` were set. Returns an empty string when none is set.
fn nfo_metadata_header(config: &Config) -> String {
    let mut header = String::new();
    if let Some(imdb_id) = &config.imdb_id {
        header.push_str(&format!("IMDb : https://www.imdb.com/title/{imdb_id}/\n"));
    }
    if let Some(tmdb_id) = &config.tmdb_id {
        header.push_str(&format!("TMDb : https://www.themoviedb.org/{tmdb_id}\n"));
    }
    if let Some(tvdb_id) = &config.tvdb_id {
        // The dereferrer link resolves by ID alone, without needing the
        // title's slug — but the path segment must still match the media
        // kind (movie vs. series), unlike a plain numeric ID.
        let kind = config
            .tvdb_kind
            .unwrap_or(pesto::nzb::TvdbKind::Series)
            .as_str();
        header.push_str(&format!(
            "TVDB : https://thetvdb.com/dereferrer/{kind}/{tvdb_id}\n"
        ));
    }
    if let Some(mal_id) = &config.mal_id {
        header.push_str(&format!("MAL  : https://myanimelist.net/anime/{mal_id}\n"));
    }
    if !header.is_empty() {
        header.push('\n');
    }
    header
}

// ── NZB metadata helpers ──────────────────────────────────────────────────────

/// Add obfuscation mode tag to NZB metadata tags.
/// This helps indexers understand what mode was used during posting.
fn add_obfuscation_tag(tags: &mut Vec<String>, obfuscate: &ObfuscateMode) {
    match obfuscate {
        ObfuscateMode::None => {}
        ObfuscateMode::Full => {
            tags.push("obfuscated:full".to_string());
        }
        ObfuscateMode::Light => {
            tags.push("obfuscated:light".to_string());
        }
        ObfuscateMode::Article => {
            tags.push("obfuscated:article".to_string());
        }
        ObfuscateMode::FullShared => {
            tags.push("obfuscated:full-shared".to_string());
        }
    }
}

/// Entry point.
///
/// Deliberately *not* `#[tokio::main]`: two things have to happen before the
/// runtime spawns its first thread, and the attribute leaves no room for
/// either.
///
/// 1. `tune_allocator()` must run before any thread exists — on glibc,
///    malloc's per-core arenas are created lazily on first allocation from a
///    new thread and can never be reclaimed afterwards.
/// 2. The runtime itself must be built with bounded thread counts and stack
///    sizes. `#[tokio::main]`'s defaults (`ncores` workers, 2 MiB stacks) are
///    the largest avoidable consumer of address space on a many-core seedbox:
///    measured over a full `--par2-only --threads 128` run, bounding them
///    takes peak address space from 803.5 MiB to 443.3 MiB. Against a typical
///    seedbox `ulimit -v` that headroom is the difference between finishing a
///    100 GiB post and aborting mid-encode. See [`pesto::memory`].
fn main() -> Result<()> {
    pesto::memory::tune_allocator();
    let tuning = pesto::memory::ThreadTuning::detect();
    let runtime = tuning
        .build_runtime()
        .context("building the tokio runtime")?;
    let result = runtime.block_on(command::run(tuning));
    // Logged from here rather than at the end of `run` so it covers the error
    // paths too. It does not cover the `std::process::exit` calls on Ctrl-C —
    // those bypass every unwind and destructor by design.
    info!("memory: {} (exit)", pesto::memory::peak_summary());
    if pesto::memory::report_enabled() {
        let ceiling = pesto::memory::Ceiling::discover(pesto::memory::explicit_memory_limit());
        println!("{}", pesto::memory::report_summary(&ceiling));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use pesto::config::{FileConfig, Overrides};

    fn resolve_with_tvdb(tvdb_id: &str) -> Config {
        let mut file = FileConfig::default();
        file.server.host = Some("news.example.com".into());
        file.posting.groups = Some(vec!["alt.test".into()]);
        Config::resolve(
            file,
            Overrides {
                tvdb_id: Some(tvdb_id.to_string()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn tvdb_bare_id_defaults_to_series_category_and_dereferrer() {
        let config = resolve_with_tvdb("81189");
        assert_eq!(config.nzb_category.as_deref(), Some("tv"));
        assert!(
            nfo_metadata_header(&config).contains("https://thetvdb.com/dereferrer/series/81189")
        );
    }

    #[test]
    fn tvdb_movie_ref_sets_movies_category_and_dereferrer() {
        let config = resolve_with_tvdb("movie/123");
        assert_eq!(config.nzb_category.as_deref(), Some("movies"));
        assert!(nfo_metadata_header(&config).contains("https://thetvdb.com/dereferrer/movie/123"));
    }

    #[test]
    fn tvdb_explicit_category_overrides_kind_default() {
        let mut file = FileConfig::default();
        file.server.host = Some("news.example.com".into());
        file.posting.groups = Some(vec!["alt.test".into()]);
        let config = Config::resolve(
            file,
            Overrides {
                tvdb_id: Some("movie/123".to_string()),
                nzb_category: Some("custom".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(config.nzb_category.as_deref(), Some("custom"));
    }
}
