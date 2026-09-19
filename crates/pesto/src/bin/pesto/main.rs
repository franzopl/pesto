//! `pesto` — fast, lean Usenet poster.
//!
//! Parses the CLI, resolves the configuration, posts the given files to Usenet
//! and writes an `.nzb` file describing the result.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use pesto::config::{self, Config, FileConfig, ObfuscateMode};
use pesto::logging;
use tracing::info;

mod batch;
mod cleanup;
mod cli;
mod hooks;
mod merge;
mod output;
mod season;
mod summary;
mod upload;
mod watch;

use batch::{derive_season_nzb_path, release_label, run_batch};
use cleanup::CleanupMode;
use cli::Cli;
use hooks::{run_all_hooks, HookEnv};
use merge::run_merge_season;
use summary::write_session_summary;
use upload::{run_single_upload, UploadParams};
use watch::{run_watch, WatchBatchOpts};

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
    let result = runtime.block_on(run(tuning));
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

async fn run(tuning: pesto::memory::ThreadTuning) -> Result<()> {
    let mut cli = Cli::parse();

    // `pesto --config` with no value: launch the interactive setup wizard.
    if matches!(cli.config, Some(None)) {
        return pesto::ui::wizard::run();
    }

    if cli.update {
        return pesto::update::run().await;
    }

    // Handle `-` (stdin) in the file list.
    // Read all of stdin into a temp file and replace the `-` path with it.
    // Only one `-` is allowed per invocation; combining with --each/--season
    // is not supported (PAR2 and compression require a real file on disk).
    let _stdin_tempfile: Option<tempfile::NamedTempFile>;
    if cli.files.iter().any(|p| p.as_os_str() == "-") {
        if cli.files.iter().filter(|p| p.as_os_str() == "-").count() > 1 {
            anyhow::bail!("stdin (`-`) may only appear once in the file list");
        }
        if cli.each || cli.season {
            anyhow::bail!("stdin (`-`) cannot be combined with --each or --season");
        }
        let stdin_name = cli
            .stdin_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("--stdin-name is required when reading from stdin (`-`)")
            })?;

        use std::io::Read;
        if std::io::stdin().is_terminal() {
            anyhow::bail!("stdin is a terminal; pipe data into pesto or use a file instead of `-`");
        }

        // Read stdin into a named temp file so poster.rs can seek and stat it.
        let mut tmp = tempfile::Builder::new()
            .prefix("pesto_stdin_")
            .tempfile()
            .context("creating stdin temp file")?;
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("reading from stdin")?;
        std::io::Write::write_all(&mut tmp, &buf).context("writing stdin to temp file")?;
        let tmp_path = tmp.path().to_path_buf();
        // Keep the temp file alive until the upload is done.
        _stdin_tempfile = Some(tmp);

        // Replace `-` with the temp path and set the published name via a
        // special sentinel that run_single_upload will recognise.
        for p in &mut cli.files {
            if p.as_os_str() == "-" {
                *p = tmp_path.clone();
            }
        }
        // Store the desired name in cli.stdin_name; run_single_upload will
        // use it when building InputFile from the temp path.
        // We rename the file itself so expand_inputs picks up the right base name.
        // Easiest: just rename the temp file to have the desired name as its last component.
        let named_tmp_dir = tmp_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/tmp"));
        let named_path = named_tmp_dir.join(stdin_name);
        // Only rename if the paths differ (avoid overwriting if name matches).
        if named_path != tmp_path {
            std::fs::hard_link(&tmp_path, &named_path)
                .or_else(|_| std::fs::copy(&tmp_path, &named_path).map(|_| ()))
                .context("naming stdin temp file")?;
            for p in &mut cli.files {
                if *p == tmp_path {
                    *p = named_path.clone();
                }
            }
        }
    } else {
        _stdin_tempfile = None;
    }

    // --out names a single fixed file; combined with --watch --season, every
    // distinct folder detected over time would silently clobber the same
    // path. Point the user at --nzb-dir instead, which names each season NZB
    // after its folder.
    anyhow::ensure!(
        !(cli.watch.is_some() && cli.season && cli.out.is_some()),
        "--out cannot be combined with --watch --season (each detected folder needs its own \
         season NZB name); use --nzb-dir instead"
    );

    // --merge-season: offline NZB merge, no server connection needed.
    if let Some(ref dir) = cli.merge_season {
        // No upload here, so no session log — just honour -v/--log-file.
        logging::init(cli.verbose, cli.log_file.as_deref(), None)?;
        let nzb_tags = if !cli.nzb_tag.is_empty() {
            cli.nzb_tag.clone()
        } else {
            let fc = match &cli.config {
                Some(Some(path)) => FileConfig::load(path).ok(),
                _ => config::default_config_path()
                    .filter(|p| p.exists())
                    .and_then(|p| FileConfig::load(&p).ok()),
            };
            fc.map(|c| c.output.nzb_tags).unwrap_or_default()
        };
        let nzb_title = cli.nzb_title.as_deref().or_else(|| {
            cli.nzb_name.as_deref().inspect(|_| {
                eprintln!(
                    "warning: --nzb-name is deprecated, use --nzb-title instead; \
                     --nzb-name will stop being accepted in a future release"
                );
            })
        });
        return run_merge_season(dir, nzb_title, nzb_tags);
    }

    // `pesto` with nothing to post and no --watch: show the orientation screen.
    let has_work = !cli.files.is_empty() || cli.watch.is_some();
    if !has_work {
        print_welcome();
        return Ok(());
    }

    print_header();
    if let Some(notice) = pesto::update::check_notice().await {
        eprintln!("{notice}");
    }

    // Resolve config file.
    let (file_config, nzb_default) = match &cli.config {
        Some(Some(path)) => (FileConfig::load(path)?, None),
        _ => {
            let default_path = config::default_config_path();
            match default_path.as_deref().filter(|p| p.exists()) {
                Some(path) => {
                    eprintln!("using config: {}", path.display());
                    let fc = FileConfig::load(path)?;
                    let nzb = fc.output.nzb.clone();
                    (fc, nzb)
                }
                // Nothing found at the OS-standard location: say exactly where
                // pesto looked, so a config placed at the wrong path (e.g.
                // ~/.config on Windows, which pesto never checks — see #43)
                // doesn't look like it's being silently ignored.
                None => {
                    match &default_path {
                        Some(path) => eprintln!(
                            "no config found at {} — using CLI flags/built-in defaults only. \
                             Run `pesto --config` to create one there.",
                            path.display()
                        ),
                        None => eprintln!(
                            "no config directory could be determined for this OS — using CLI \
                             flags/built-in defaults only."
                        ),
                    }
                    (FileConfig::default(), None)
                }
            }
        }
    };
    let nzb_default = nzb_default.or_else(|| file_config.output.nzb.clone());
    // Read before `file_config` is consumed by `Config::resolve`.
    let session_log_enabled = !cli.no_session_log && file_config.output.session_log.unwrap_or(true);
    let config = Arc::new(Config::resolve(file_config, cli.overrides())?);
    let json_mode = cli.output_format.trim().eq_ignore_ascii_case("json");

    // Initialise logging now that the history directory is known. The verbose
    // (`-v`) output goes to stderr or --log-file as before; in parallel, unless
    // disabled, every upload also writes a DEBUG log to `<history_dir>/logs/`
    // so it can be analysed afterwards without re-running with -vv.
    let session_log = if session_log_enabled {
        let name = cli
            .files
            .iter()
            .find(|p| p.as_os_str() != "-")
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .or_else(|| cli.watch.as_ref().map(|_| "watch".to_string()))
            .unwrap_or_else(|| "pesto".to_string());
        pesto::history::session_log_path(config.history_dir.as_deref(), &name, 50)
    } else {
        None
    };
    logging::init(cli.verbose, cli.log_file.as_deref(), session_log.as_deref())?;
    logging::log_system_info();
    // Logged unconditionally at INFO (not gated on -vv): when a run dies to
    // `handle_alloc_error` there is no unwind and no further output, so the
    // last line already in the log is the only evidence of how much address
    // space was left. See `pesto::memory`.
    info!(
        worker_threads = tuning.worker_threads,
        max_blocking_threads = tuning.max_blocking_threads,
        thread_stack_kib = tuning.thread_stack_size / 1024,
        "memory: {} (startup)",
        pesto::memory::footprint_summary(),
    );
    // Started after logging is up so `--memory-trace` output has somewhere to
    // go. Peak-and-stage tracking (and pressure-level logging) is always on;
    // only the per-sample trace line is gated on the flag. `Ceiling` is cheap
    // to (re)compute — see `memory::Ceiling::discover` — so it's read fresh
    // here rather than threaded through from anywhere earlier. It does need
    // `config.memory_limit` (the resolved global `--memory-limit`, if any)
    // so the sampler's pressure percentages and `--memory-report`'s
    // breakdown agree with what `producer` actually enforced.
    pesto::memory::set_report_enabled(cli.memory_report);
    pesto::memory::set_explicit_memory_limit(config.memory_limit);
    pesto::memory::start_sampler(
        cli.memory_trace,
        pesto::memory::Ceiling::discover(config.memory_limit),
    );
    if let Some(p) = &session_log {
        tracing::debug!(path = %p.display(), "session log");
    }

    // Fall back to the append-only plain renderer whenever verbose logs share
    // stderr with the panel — at *any* -v level, not just -vv: an INFO-level
    // `-v` run also writes connection/pool lines to stderr, which the panel's
    // cursor-movement redraws would shred (and be shredded by). If the user
    // redirected logs to a file with --log-file the panel can run safely.
    let logs_to_stderr = cli.verbose >= 1 && cli.log_file.is_none();

    // Validate mutually exclusive flags.
    anyhow::ensure!(
        !(cli.cleanup && cli.cleanup_to.is_some()),
        "--cleanup and --cleanup-to are mutually exclusive"
    );

    let cleanup_mode = if cli.cleanup {
        CleanupMode::Delete
    } else if let Some(dir) = cli.cleanup_to.clone() {
        CleanupMode::MoveTo(dir)
    } else {
        CleanupMode::Leave
    };

    let params = Arc::new(UploadParams {
        config: Arc::clone(&config),
        archive_password_raw: cli.archive_password.clone(),
        nzb_default: nzb_default.map(|s| s.to_string()),
        json_mode,
        out: cli.out.clone(),
        write_history: config.history,
        renderer_opts: pesto::progress::RendererOptions {
            quiet: cli.quiet || config.quiet,
            bell: cli.bell || config.bell,
            plain: logs_to_stderr,
        },
        ext_filter: cli
            .ext
            .iter()
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .collect(),
        cleanup_mode,
    });

    // Unified cancellation flag: one signal listener for the whole process.
    let cancel = Arc::new(AtomicBool::new(false));
    pesto::cancel::spawn_listener(cancel.clone());

    // ── --watch mode ──────────────────────────────────────────────────────────
    if let Some(watch_dir) = &cli.watch {
        let any_cancelled = run_watch(
            params,
            watch_dir,
            cli.watch_done.as_deref(),
            cli.watch_interval,
            cli.jobs,
            WatchBatchOpts {
                each: cli.each,
                season: cli.season,
                explicit_out: cli.out.clone(),
            },
            cancel,
        )
        .await?;
        if any_cancelled {
            std::process::exit(130);
        }
        return Ok(());
    }

    // ── --each / --season batch mode ─────────────────────────────────────────
    let batch_mode = cli.each || cli.season;
    if batch_mode {
        // For --season, derive the consolidated NZB path from the first directory arg.
        let season_nzb: Option<PathBuf> = if cli.season {
            cli.out.clone().or_else(|| {
                cli.files
                    .iter()
                    .find(|p| std::fs::metadata(p).map(|md| md.is_dir()).unwrap_or(false))
                    .map(|entry| {
                        derive_season_nzb_path(None, entry, params.config.nzb_dir.as_deref())
                    })
            })
        } else {
            None
        };

        let (_, any_cancelled, any_failures) =
            run_batch(params, &cli.files, cli.jobs, season_nzb, cancel).await?;

        if any_cancelled {
            std::process::exit(130);
        }
        if any_failures {
            std::process::exit(1);
        }
        return Ok(());
    }

    // ── Single upload (normal mode) ───────────────────────────────────────────
    // Same label helper as `--season`/`--each` (no is_dir/stat on the
    // async executor). See `release_label`.
    let label = cli
        .files
        .first()
        .map(|p| release_label(p))
        .unwrap_or_else(|| format!("{}", std::process::id()));
    let result = run_single_upload(
        &params,
        &cli.files,
        &label,
        Some(&cancel),
        None,
        false,
        None,
    )
    .await?;

    if let Some(ref p) = session_log {
        write_session_summary(
            p,
            &label,
            result.cancelled,
            result.had_failures,
            result.total_bytes,
            result.nzb_path.as_deref(),
        );
    }

    if result.cancelled {
        std::process::exit(130);
    }
    if result.had_failures {
        std::process::exit(1);
    }
    Ok(())
}

/// Print the orientation screen shown when `pesto` is run with no files.
fn print_header() {
    eprintln!(
        "pesto v{} — fast, lean Usenet poster",
        pesto::DISPLAY_VERSION
    );
    eprintln!("{}", "─".repeat(48));
}

fn print_welcome() {
    let cfg = config::default_config_path();
    let cfg_exists = cfg.as_deref().map(Path::exists).unwrap_or(false);

    println!("pesto — fast, lean Usenet poster\n");
    println!("Getting started:");
    println!("  pesto <PATH>...     post files or directories to Usenet");
    println!("  pesto --config      create your config with a guided wizard");
    println!("  pesto --help        show every option in detail\n");

    match (&cfg, cfg_exists) {
        (Some(path), true) => println!("Config found: {}", path.display()),
        (Some(path), false) => {
            println!("No config yet. Run `pesto --config` to create one at:");
            println!("  {}", path.display());
        }
        (None, _) => println!(
            "Set $HOME or $XDG_CONFIG_HOME so pesto can locate a config file,\n\
             or pass every setting as a flag (see `pesto --help`)."
        ),
    }
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
