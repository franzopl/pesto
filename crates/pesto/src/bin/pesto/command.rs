//! CLI configuration, validation and top-level mode dispatch.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use pesto::config::{self, Config, FileConfig};
use pesto::logging;
use tracing::info;

use super::batch::{derive_season_nzb_path, release_label, run_batch};
use super::cleanup::CleanupMode;
use super::cli::Cli;
use super::merge::run_merge_season;
use super::summary::write_session_summary;
use super::upload::{run_single_upload, UploadParams};
use super::watch::{run_watch, WatchBatchOpts};

pub(super) async fn run(tuning: pesto::memory::ThreadTuning) -> Result<()> {
    let mut cli = Cli::parse();

    // `pesto --config` with no value: launch the interactive setup wizard.
    if matches!(cli.config, Some(None)) {
        return pesto::ui::wizard::run();
    }

    if cli.update {
        return pesto::update::run().await;
    }

    // Keep the temporary file alive until mode dispatch completes.
    let _stdin_tempfile = materialize_stdin(&mut cli)?;

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
        return run_merge_command(&cli, dir);
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

    let (file_config, nzb_default) = load_file_config(&cli)?;
    let nzb_default = nzb_default.or_else(|| file_config.output.nzb.clone());
    // Read before `file_config` is consumed by `Config::resolve`.
    let session_log_enabled = !cli.no_session_log && file_config.output.session_log.unwrap_or(true);
    // Q-A: bare `--encrypt` honours a configured `[encryption] password`;
    // a random password is generated only when none is configured.
    prepare_encrypt_password(
        &mut cli,
        file_config.encryption.password.as_deref(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )?;
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

    let cleanup_mode = cleanup_mode(&cli)?;

    let params = Arc::new(UploadParams {
        config: Arc::clone(&config),
        exclusion_root: None,
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
        ext_filter: config.ext.clone(),
        cleanup_mode,
    });

    dispatch_upload(&cli, params, session_log.as_deref()).await
}

/// Materialize a single stdin input as a seekable named temporary file.
fn materialize_stdin(cli: &mut Cli) -> Result<Option<tempfile::NamedTempFile>> {
    if !cli.files.iter().any(|p| p.as_os_str() == "-") {
        return Ok(None);
    }
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
        .ok_or_else(|| anyhow::anyhow!("--stdin-name is required when reading from stdin (`-`)"))?;

    use std::io::Read;
    if std::io::stdin().is_terminal() {
        anyhow::bail!("stdin is a terminal; pipe data into pesto or use a file instead of `-`");
    }

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

    for path in &mut cli.files {
        if path.as_os_str() == "-" {
            *path = tmp_path.clone();
        }
    }
    let named_tmp_dir = tmp_path.parent().unwrap_or_else(|| Path::new("/tmp"));
    let named_path = named_tmp_dir.join(stdin_name);
    if named_path != tmp_path {
        std::fs::hard_link(&tmp_path, &named_path)
            .or_else(|_| std::fs::copy(&tmp_path, &named_path).map(|_| ()))
            .context("naming stdin temp file")?;
        for path in &mut cli.files {
            if *path == tmp_path {
                *path = named_path.clone();
            }
        }
    }
    Ok(Some(tmp))
}

fn run_merge_command(cli: &Cli, dir: &Path) -> Result<()> {
    // No upload here, so no session log — just honour -v/--log-file.
    logging::init(cli.verbose, cli.log_file.as_deref(), None)?;
    let nzb_tags = if !cli.nzb_tag.is_empty() {
        cli.nzb_tag.clone()
    } else {
        let file_config = match &cli.config {
            Some(Some(path)) => FileConfig::load(path).ok(),
            _ => config::default_config_path()
                .filter(|path| path.exists())
                .and_then(|path| FileConfig::load(&path).ok()),
        };
        file_config
            .map(|config| config.output.nzb_tags)
            .unwrap_or_default()
    };
    let nzb_title = cli.nzb_title.as_deref().or_else(|| {
        cli.nzb_name.as_deref().inspect(|_| {
            eprintln!(
                "warning: --nzb-name is deprecated, use --nzb-title instead; \
                 --nzb-name will stop being accepted in a future release"
            );
        })
    });
    run_merge_season(dir, nzb_title, nzb_tags)
}

fn load_file_config(cli: &Cli) -> Result<(FileConfig, Option<String>)> {
    if let Some(Some(path)) = &cli.config {
        return Ok((FileConfig::load(path)?, None));
    }

    let default_path = config::default_config_path();
    if let Some(path) = default_path.as_deref().filter(|path| path.exists()) {
        eprintln!("using config: {}", path.display());
        let file_config = FileConfig::load(path)?;
        let nzb = file_config.output.nzb.clone();
        return Ok((file_config, nzb));
    }

    // Report the exact missing location so an incorrectly placed config does
    // not look like it was silently ignored (see #43).
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
    Ok((FileConfig::default(), None))
}

fn cleanup_mode(cli: &Cli) -> Result<CleanupMode> {
    anyhow::ensure!(
        !(cli.cleanup && cli.cleanup_to.is_some()),
        "--cleanup and --cleanup-to are mutually exclusive"
    );
    Ok(if cli.cleanup {
        CleanupMode::Delete
    } else if let Some(dir) = cli.cleanup_to.clone() {
        CleanupMode::MoveTo(dir)
    } else {
        CleanupMode::Leave
    })
}

async fn dispatch_upload(
    cli: &Cli,
    params: Arc<UploadParams>,
    session_log: Option<&Path>,
) -> Result<()> {
    let cancel = Arc::new(AtomicBool::new(false));
    pesto::cancel::spawn_listener(cancel.clone());

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

    if let Some(p) = session_log {
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

// Keep password generation at command dispatch; querying overrides has no output.
// Q-A: a bare `--encrypt` resolves to a configured `[encryption] password`
// when one is set (generation happens only when none is configured). The
// configured value is copied into `cli.encrypt` because `Config::resolve`
// prefers the CLI value (`cli.or(file...)`); leaving the bare marker `""`
// there would shadow the config and fail validation as an empty password.
// An explicit `--encrypt=PASS` is a non-empty value and is untouched.
fn prepare_encrypt_password(
    cli: &mut Cli,
    configured_password: Option<&str>,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
) -> Result<()> {
    if cli.encrypt.as_deref() == Some("") {
        if let Some(configured) = configured_password {
            cli.encrypt = Some(configured.to_string());
        } else {
            let pass = pesto::compress::random_password();
            if cli.output_format.trim().eq_ignore_ascii_case("json") {
                writeln!(stderr, "encryption password: {pass}")?;
            } else {
                writeln!(stdout, "encryption password: {pass}")?;
            }
            cli.encrypt = Some(pass);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_kdf_input() -> &'static str {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test-vectors/body_encryption.json");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let fixture: serde_json::Value =
            serde_json::from_str(&content).expect("body_encryption.json must parse");
        static VALUE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        VALUE
            .get_or_init(|| {
                fixture["vectors"][0]["password"]
                    .as_str()
                    .expect("body_encryption.json vectors[0].password")
                    .to_owned()
            })
            .as_str()
    }

    #[test]
    fn bare_encrypt_preserves_positional_files() {
        let cli = Cli::try_parse_from(["pesto", "--encrypt", "file.mkv"]).unwrap();
        assert_eq!(cli.encrypt, Some("".to_string()));
        assert_eq!(cli.files, vec![PathBuf::from("file.mkv")]);
    }

    #[test]
    fn explicit_encrypt_with_equals_preserves_positional_files() {
        let cli = Cli::try_parse_from([
            "pesto",
            format!("--encrypt={}", fixture_kdf_input()).as_str(),
            "file.mkv",
        ])
        .unwrap();
        assert_eq!(cli.encrypt.as_deref(), Some(fixture_kdf_input()));
        assert_eq!(cli.files, vec![PathBuf::from("file.mkv")]);
    }

    #[test]
    fn encrypt_password_spelling_is_an_unknown_arg_error() {
        // DEC-1: `--encrypt[=PASS]` is the only form. The removed
        // `--encrypt-password` alias must not swallow `x` as its value or
        // as an input file; any form of it is an unknown-arg error.
        let err = Cli::try_parse_from(["pesto", "--encrypt-password=x", "file.mkv"]).unwrap_err();
        assert!(
            err.to_string().contains("--encrypt-password"),
            "unknown-arg error must name the removed alias, got: {err}"
        );
        assert!(!err.to_string().contains("random"), "got: {err}");
    }

    #[test]
    fn bare_encrypt_prints_generated_password_once() {
        for format in ["text", "json"] {
            let mut cli =
                Cli::try_parse_from(["pesto", "--encrypt", "--output-format", format, "file.bin"])
                    .unwrap();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            prepare_encrypt_password(&mut cli, None, &mut stdout, &mut stderr).unwrap();
            let password = cli.overrides().encrypt_password.unwrap();
            assert!(!password.is_empty());
            prepare_encrypt_password(&mut cli, None, &mut stdout, &mut stderr).unwrap();
            assert_eq!(
                cli.overrides().encrypt_password.as_deref(),
                Some(password.as_str())
            );
            let expected = format!("encryption password: {password}\n");
            let (output, unused) = if format == "json" {
                (&stderr, &stdout)
            } else {
                (&stdout, &stderr)
            };
            assert_eq!(output, expected.as_bytes());
            assert!(unused.is_empty());
        }
    }

    #[test]
    fn bare_encrypt_honours_configured_password() {
        // Q-A / R11-PU-03: with `[encryption] password` configured, a bare
        // `--encrypt` must not generate or print a random password, and the
        // resolved config must carry the configured password.
        let mut cli = Cli::try_parse_from(["pesto", "--encrypt", "file.bin"]).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut file = FileConfig::default();
        file.server.host = Some("news.example.com".into());
        file.posting.groups = Some(vec!["alt.test".into()]);
        file.encryption.password = Some("conformance-fixture-configured-password".into());
        prepare_encrypt_password(
            &mut cli,
            file.encryption.password.as_deref(),
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        assert!(stdout.is_empty(), "no generated password may be printed");
        assert!(stderr.is_empty(), "no generated password may be printed");
        let config = Config::resolve(file, cli.overrides()).unwrap();
        assert_eq!(
            config.encrypt_password.as_deref(),
            Some("conformance-fixture-configured-password")
        );
    }

    #[test]
    fn explicit_encrypt_password_wins_over_configured() {
        // An explicit `--encrypt=PASS` still wins over config (unchanged).
        let mut cli = Cli::try_parse_from([
            "pesto",
            format!("--encrypt={}", fixture_kdf_input()).as_str(),
            "file.bin",
        ])
        .unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut file = FileConfig::default();
        file.server.host = Some("news.example.com".into());
        file.posting.groups = Some(vec!["alt.test".into()]);
        file.encryption.password = Some("conformance-fixture-configured-password".into());
        prepare_encrypt_password(
            &mut cli,
            file.encryption.password.as_deref(),
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        assert_eq!(cli.encrypt.as_deref(), Some(fixture_kdf_input()));
        assert!(stdout.is_empty() && stderr.is_empty());
        let config = Config::resolve(file, cli.overrides()).unwrap();
        assert_eq!(
            config.encrypt_password.as_deref(),
            Some(fixture_kdf_input())
        );
    }
}
