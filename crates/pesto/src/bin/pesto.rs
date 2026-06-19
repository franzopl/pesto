//! `pesto` — fast, lean Usenet poster.
//!
//! Parses the CLI, resolves the configuration, posts the given files to Usenet
//! and writes an `.nzb` file describing the result.

use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use parmesan::SimdPath;
use pesto::compress::{compress, random_password, ArchiveFormat};
use pesto::config::{self, parse_upload_rate, Config, FileConfig, ObfuscateMode, Overrides};
use pesto::logging;
use pesto::nzb::NzbMeta;
use pesto::poster::PostedSegment;
use tracing::{error, info, warn};

/// One-line summary shown at the top of `--help`.
const ABOUT: &str = "Fast, lean Usenet poster: yEnc-encode files, post over NNTP, emit an .nzb.";

/// Extended description shown by `pesto --help`.
const LONG_ABOUT: &str = "\
pesto posts files to Usenet. It yEnc-encodes each file, uploads the articles
over parallel NNTP connections and writes an .nzb describing what was posted.

A PATH argument may be a directory: it is walked recursively and the whole
tree is posted as one upload, with the folder structure preserved in the .nzb
and PAR2 metadata.

Server and credentials are read from a TOML config file. If --config is not
given, pesto loads $XDG_CONFIG_HOME/pesto/config.toml (or, failing that,
~/.config/pesto/config.toml) so a single setup serves every run. Create that
file interactively with `pesto --config`.

Any config value can be overridden by the matching flag below.";

/// Examples printed after the option list.
const AFTER_HELP: &str = "\
EXAMPLES:
  pesto movie.mkv                 post one file using the saved config
  pesto ./Season01/               post a whole directory, structure preserved
  pesto --config                  create the config file with a guided wizard
  pesto --out up.nzb a.bin b.bin  post two files and write an .nzb
  pesto --par2 15 movie.mkv       post with 15% PAR2 recovery data
  pesto --dry-run movie.mkv       encode only, never touch the network
  pesto --each ./Season01/        post each episode as a separate upload
  pesto --season ./Season01/      post each episode + a combined season NZB
  pesto --each --jobs 4 ./shows/  post up to 4 entries in parallel
  pesto --watch ./incoming/       watch a folder and post new entries

By default pesto posts under a freshly generated random identity. Set
[posting].from (or --from) only if you need a fixed one.";

#[derive(Parser, Debug)]
#[command(
    name = "pesto",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    after_help = AFTER_HELP
)]
struct Cli {
    /// TOML config file to load. With no value (`pesto --config`), launch the
    /// interactive setup wizard instead. When omitted, the default config
    /// path is used if it exists.
    #[arg(short, long, value_name = "PATH", num_args = 0..=1)]
    config: Option<Option<PathBuf>>,

    /// NNTP server hostname [config: server.host].
    #[arg(long, value_name = "HOST")]
    host: Option<String>,

    /// NNTP server port [config: server.port, default 563].
    #[arg(long, value_name = "PORT")]
    port: Option<u16>,

    /// Disable TLS; connect in plaintext [config: server.ssl].
    #[arg(long)]
    no_ssl: bool,

    /// Number of parallel connections [config: server.connections, default 4].
    #[arg(long, value_name = "N")]
    connections: Option<usize>,

    /// Authentication username [config: auth.username].
    #[arg(long, value_name = "USER")]
    username: Option<String>,

    /// Authentication password for the NNTP server [config: auth.password].
    #[arg(long = "auth-password", value_name = "PASS")]
    password: Option<String>,

    /// `From` header for posted articles; omitted means a random identity
    /// [config: posting.from].
    #[arg(long, value_name = "ADDRESS")]
    from: Option<String>,

    /// Newsgroups to post to (repeat or comma-separate) [config: posting.groups].
    #[arg(long, value_name = "GROUP", value_delimiter = ',')]
    groups: Vec<String>,

    /// Target size of each article body, in bytes
    /// [config: posting.article_size, default 768000].
    #[arg(long, value_name = "BYTES")]
    article_size: Option<usize>,

    /// yEnc line length, in encoded characters
    /// [config: posting.line_length, default 128].
    #[arg(long, value_name = "CHARS")]
    line_length: Option<usize>,

    /// Post attempts per segment before it is marked failed
    /// [config: posting.retries, default 3].
    #[arg(long, value_name = "N")]
    retries: Option<u32>,

    /// Articles to pipeline per connection before reading responses.
    /// 0 (default) = adaptive: measures RTT on the first article and computes
    /// the optimal depth automatically (max 8). Set to 1 for sequential.
    /// Incompatible with --verify [config: posting.pipeline_depth, default 0].
    #[arg(long, value_name = "N")]
    pipeline_depth: Option<usize>,

    /// Seconds to wait between failed post attempts
    /// [config: server.retry_delay, default 1].
    #[arg(long, value_name = "SECS")]
    retry_delay: Option<u64>,

    /// Path of the `.nzb` file to write [config: output.nzb].
    #[arg(short, long, value_name = "PATH")]
    out: Option<PathBuf>,

    /// Directory where `.nzb` files are saved; filename derived from upload
    /// name [config: output.nzb_dir]. Overridden by --out.
    #[arg(long, value_name = "DIR")]
    nzb_dir: Option<PathBuf>,

    /// Obfuscation mode: `none`, `full`. A bare `--obfuscate` means `full`
    /// [config: posting.obfuscate, default none].
    #[arg(long, value_name = "MODE", value_enum, num_args = 0..=1,
          default_missing_value = "full", require_equals = true)]
    obfuscate: Option<ObfuscateMode>,

    /// Percentage of PAR2 recovery data to generate; 0 disables it
    /// [config: posting.par2, default 10].
    #[arg(long, value_name = "PERCENT")]
    par2: Option<u8>,

    /// Manual PAR2 slice size, e.g. "1 MiB" [default: auto].
    #[arg(long, value_name = "SIZE")]
    slice_size: Option<String>,

    /// Target number of PAR2 input slices [default: auto].
    #[arg(long, value_name = "N")]
    slice_count: Option<usize>,

    /// Exact number of PAR2 recovery blocks to generate [default: auto].
    #[arg(long, value_name = "N")]
    recovery_count: Option<usize>,

    /// Maximum RAM for PAR2 recovery buffers, e.g. "512 MiB"
    /// [config: posting.par2_memory_limit, default "1 GiB"].
    #[arg(long, value_name = "SIZE")]
    memory_limit: Option<String>,

    /// Number of threads for parallel PAR2 compute
    /// [default: physical cores].
    #[arg(long, value_name = "N")]
    threads: Option<usize>,

    /// Force a specific SIMD multiplication backend for PAR2.
    #[arg(long, value_enum, value_name = "MODE", default_value_t = SimdPath::Auto)]
    simd: SimdPath,

    /// Only generate PAR2 files next to the sources; do not post.
    #[arg(long)]
    par2_only: bool,

    /// Skip network posting and just measure generation speed.
    #[arg(long)]
    dry_run: bool,

    /// Resume an interrupted upload from where it left off. Without this flag
    /// pesto always starts fresh, even if a state file exists
    /// [config: output.resume = true].
    #[arg(long)]
    resume: bool,

    /// After posting each article, confirm it is present on the server with
    /// STAT and repost if not found [config: posting.verify, default false].
    #[arg(long)]
    verify: bool,

    /// Maximum upload rate across all connections (e.g. "50 MiB/s", "10 MB/s").
    /// 0 or omitted means unlimited [config: posting.upload_rate].
    #[arg(long, value_name = "RATE")]
    rate: Option<String>,

    /// Bundle all files into an archive before posting. Optional FORMAT:
    /// `7z` (default, store mode), `zip` (via 7z), or `rar` (requires rar in
    /// PATH) [config: compression.format].
    #[arg(long, value_name = "FORMAT", num_args = 0..=1, default_missing_value = "7z")]
    compress: Option<String>,

    /// Bundle files into a password-protected archive before posting. Optional
    /// PASSWORD: bare `--password` generates a random 24-character password
    /// and prints it; `--password=mypass` uses an explicit one. Implies
    /// `--compress` with the configured or default format.
    #[arg(long = "password", value_name = "PASSWORD",
          num_args = 0..=1, default_missing_value = "")]
    archive_password: Option<String>,

    /// Friendly display name emitted as `<meta type="name">` in the `.nzb`
    /// (shown by NZBGet / SABnzbd) [config: output.nzb_name].
    #[arg(long, value_name = "NAME")]
    nzb_name: Option<String>,

    /// Extraction password written to `<meta type="password">` in the `.nzb`;
    /// defaults to the archive password when `--password` is set
    /// [config: output.nzb_password].
    #[arg(long, value_name = "PASS")]
    nzb_password: Option<String>,

    /// Category written to `<meta type="category">` in the `.nzb`
    /// [config: output.nzb_category].
    #[arg(long, value_name = "CAT")]
    nzb_category: Option<String>,

    /// Tag written to `<meta type="tag">` in the `.nzb`; can be repeated
    /// multiple times [config: output.nzb_tags].
    /// When this flag is used on the command line, it replaces any tags set in
    /// the config file (they are not merged).
    #[arg(long, value_name = "TAG", action = clap::ArgAction::Append)]
    nzb_tag: Vec<String>,

    /// `Date:` header for each article: `now` (current time), `random`
    /// (random time within the last 2 hours), or a fixed RFC 2822 timestamp.
    /// Omit to let the server supply the date. When obfuscation is active
    /// and no date is set, the default changes to `random` [config: posting.date].
    #[arg(long, value_name = "DATE")]
    date: Option<String>,

    /// Add `X-No-Archive: yes` to every posted article to request that
    /// servers and search engines do not archive the post
    /// [config: posting.no_archive].
    #[arg(long)]
    no_archive: bool,

    /// Fixed domain component for generated `Message-ID` headers
    /// (e.g. `example.com`). When omitted a random domain is generated per
    /// article [config: posting.message_id_domain].
    #[arg(long, value_name = "DOMAIN")]
    message_id_domain: Option<String>,

    /// Output format: `terminal` (default human-readable panel) or `json`
    /// (newline-delimited JSON events on stdout, for machine consumers like
    /// `upapasta`).
    #[arg(long, value_name = "FORMAT", default_value = "terminal")]
    output_format: String,

    /// Generate a `.nfo` file next to the `.nzb` after posting. The file
    /// contains `mediainfo` output for the first media file, or a directory
    /// listing when no video file is found [config: output.nfo = true].
    #[arg(long)]
    nfo: bool,

    /// When the user-destination `.nzb` already exists, rename it instead of
    /// overwriting (`--no-overwrite` is short for `--nzb-conflict=rename`)
    /// [config: output.nzb_conflict].
    #[arg(long)]
    no_overwrite: bool,

    /// How to handle a conflict when the user-destination `.nzb` already exists:
    /// `overwrite` (default), `rename` (append `-1`, `-2`, …), `fail`
    /// [config: output.nzb_conflict].
    #[arg(long, value_name = "MODE")]
    nzb_conflict: Option<pesto::config::NzbConflict>,

    /// Shell command to execute before the upload begins. If the command exits
    /// with a non-zero code the upload is aborted immediately. The command
    /// receives the same environment variables as the post-hook, except
    /// `PESTO_NZB` and `PESTO_NFO` (which don't exist yet at this point):
    /// `PESTO_NAME`, `PESTO_BYTES`, `PESTO_INPUT_PATHS`,
    /// `PESTO_GROUP`, `PESTO_SERVER`
    /// [config: output.pre_hook].
    #[arg(long, value_name = "CMD")]
    pre_hook: Option<String>,

    /// Shell command to execute after each successful upload. The command
    /// receives upload details via environment variables:
    /// `PESTO_NZB`, `PESTO_NFO`, `PESTO_NAME`, `PESTO_BYTES`,
    /// `PESTO_INPUT_PATHS`, `PESTO_GROUP`, `PESTO_PASSWORD`, `PESTO_SERVER`
    /// [config: output.post_hook].
    #[arg(long, value_name = "CMD")]
    post_hook: Option<String>,

    /// Skip the hook scripts in ~/.config/pesto/hooks/ for this run.
    /// The --post-hook and --pre-hook flags are unaffected and still execute.
    #[arg(long)]
    no_hooks: bool,

    /// Skip writing to the upload history catalog for this run
    /// [config: output.history = false].
    #[arg(long)]
    no_history: bool,

    /// Enable completion notifications for this run even if not configured
    /// in the config file [config: [notify]].
    #[arg(long)]
    notify: bool,

    /// Suppress completion notifications for this run
    /// [config: [notify].webhook_url / ntfy_topic].
    #[arg(long)]
    no_notify: bool,

    /// Show only a single spinning line instead of the full progress panel.
    /// Ideal for tmux / screen sessions [config: output.quiet].
    #[arg(short, long)]
    quiet: bool,

    /// Ring the terminal bell on completion [config: output.bell].
    #[arg(long)]
    bell: bool,

    /// Treat each top-level entry in a directory argument as an independent
    /// upload with its own NZB. PAR2 and NZB naming follow the entry name.
    /// Combine with --jobs for parallel uploads.
    #[arg(long)]
    each: bool,

    /// Like --each, but also produces one consolidated NZB for the whole
    /// directory. The consolidated NZB is named after the directory.
    #[arg(long)]
    season: bool,

    /// Number of independent uploads to run in parallel when --each or
    /// --season is active. Default 1 (sequential). 0 means one per logical CPU.
    #[arg(long, value_name = "N", default_value = "1")]
    jobs: usize,

    /// Watch DIR for new entries and post each one automatically, implying
    /// --each. On completion each entry is moved to --watch-done (if set);
    /// otherwise it is left in place.
    /// Exits cleanly on SIGTERM / Ctrl-C after finishing any in-progress upload.
    #[arg(long, value_name = "DIR")]
    watch: Option<PathBuf>,

    /// Destination directory for entries processed by --watch. When omitted,
    /// completed entries are left in place.
    #[arg(long, value_name = "DIR")]
    watch_done: Option<PathBuf>,

    /// How often (in seconds) to poll the watched directory for new entries
    /// [default: 30].
    #[arg(long, value_name = "SECS", default_value = "30")]
    watch_interval: u64,

    /// After all articles are posted, verify each one is present on the server
    /// via STAT. Waits --check-delay seconds before checking. Articles not
    /// found after --check-retries attempts are reported as missing
    /// [config: posting.check, default false].
    #[arg(long)]
    check: bool,

    /// Seconds to wait after the last article is posted before running the
    /// STAT verification pass [config: posting.check_delay, default 30].
    #[arg(long, value_name = "SECS")]
    check_delay: Option<u64>,

    /// Number of STAT attempts per article during post-check before marking
    /// it as missing; 20 seconds between each retry [config: posting.check_retries, default 3].
    #[arg(long, value_name = "N")]
    check_retries: Option<u32>,

    /// Number of parallel NNTP connections for the post-check STAT pass;
    /// defaults to the same value as the upload connection count
    /// [config: posting.check_connections].
    #[arg(long, value_name = "N")]
    check_connections: Option<usize>,

    /// Name to use when reading from stdin (`-`). Required when a `-` path is
    /// given; determines the filename in the NZB and PAR2 metadata.
    #[arg(long, value_name = "NAME")]
    stdin_name: Option<String>,

    /// Increase log verbosity. Repeat for more detail:
    ///   `-v` = INFO (worker state, file discovery, PAR2 geometry),
    ///   `-vv` = DEBUG (NNTP commands and responses — credentials masked),
    ///   `-vvv` = TRACE (fine-grained timing and buffer events).
    /// Logs are written to stderr (or --log-file). `RUST_LOG` overrides the
    /// level when set.
    #[arg(short, long, action = clap::ArgAction::Count, value_name = "LEVEL")]
    verbose: u8,

    /// Redirect verbose log output to FILE instead of stderr. The terminal
    /// progress panel is kept active when this flag is set. Has no effect
    /// without -v.
    #[arg(long, value_name = "FILE")]
    log_file: Option<PathBuf>,

    /// Disable the per-upload DEBUG log normally saved under
    /// `<history_dir>/logs/` [config: output.session_log, default on].
    #[arg(long)]
    no_session_log: bool,

    /// Merge all per-episode NZBs in DIR into one combined season NZB and exit.
    /// No server connection is required. NZBs are grouped by their season
    /// identifier (e.g. `S02`); each group produces one output NZB written
    /// beside the source files. Use `--nzb-name` to override the display name
    /// in the NZB `<head>`.
    #[arg(long, value_name = "DIR", conflicts_with = "files")]
    merge_season: Option<PathBuf>,

    /// Files or directories to post. A directory is walked recursively and
    /// every file inside it is posted, keeping the folder structure.
    /// Use `-` to read from stdin (requires --stdin-name).
    #[arg(value_name = "PATH")]
    files: Vec<PathBuf>,
}

impl Cli {
    /// Build config [`Overrides`] from the parsed flags.
    fn overrides(&self) -> Overrides {
        Overrides {
            host: self.host.clone(),
            port: self.port,
            // `--no-ssl` is the only TLS flag; absent means "defer to config".
            ssl: if self.no_ssl { Some(false) } else { None },
            connections: self.connections,
            username: self.username.clone(),
            password: self.password.clone(),
            from: self.from.clone(),
            groups: if self.groups.is_empty() {
                None
            } else {
                Some(self.groups.clone())
            },
            article_size: self.article_size,
            line_length: self.line_length,
            retries: self.retries,
            retry_delay: self.retry_delay,
            obfuscate: self.obfuscate,
            dry_run: if self.dry_run { Some(true) } else { None },
            par2: self.par2,
            par2_only: if self.par2_only { Some(true) } else { None },
            par2_memory_limit: self
                .memory_limit
                .as_ref()
                .and_then(|s| parse_upload_rate(s).ok()),
            par2_slice_size: self
                .slice_size
                .as_ref()
                .and_then(|s| parse_upload_rate(s).ok()),
            par2_slice_count: self.slice_count,
            par2_recovery_count: self.recovery_count,
            threads: self.threads,
            simd: Some(self.simd),
            resume: if self.resume { Some(true) } else { None },
            verify: if self.verify { Some(true) } else { None },
            upload_rate: self
                .rate
                .as_deref()
                .map(parse_upload_rate)
                .transpose()
                .unwrap_or(None),
            compress_format: self.compress.clone(),
            // None → no password; Some("") → bare --password → random;
            // Some(s) → explicit password.
            compress_password: self.archive_password.as_deref().map(|pw| {
                if pw.is_empty() {
                    random_password()
                } else {
                    pw.to_string()
                }
            }),
            nzb_name: self.nzb_name.clone(),
            nzb_password: self.nzb_password.clone(),
            nzb_category: self.nzb_category.clone(),
            nzb_tags: self.nzb_tag.clone(),
            nzb_dir: self
                .nzb_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            history: if self.no_history { Some(false) } else { None },
            notify: if self.no_notify {
                Some(false)
            } else if self.notify {
                Some(true)
            } else {
                None
            },
            date: self.date.clone(),
            no_archive: if self.no_archive { Some(true) } else { None },
            message_id_domain: self.message_id_domain.clone(),
            pre_hook: self.pre_hook.clone(),
            post_hook: self.post_hook.clone(),
            no_hooks: self.no_hooks,
            nfo: if self.nfo { Some(true) } else { None },
            nzb_conflict: if self.no_overwrite {
                Some(pesto::config::NzbConflict::Rename)
            } else {
                self.nzb_conflict
            },
            check: if self.check { Some(true) } else { None },
            check_delay_secs: self.check_delay,
            check_retries: self.check_retries,
            check_connections: self.check_connections,
            pipeline_depth: self.pipeline_depth,
        }
    }
}

/// Parameters for a single upload job that don't change between entries.
struct UploadParams {
    config: Arc<Config>,
    /// The raw `--password` flag value (used to detect "was it auto-generated?").
    archive_password_raw: Option<String>,
    nzb_default: Option<String>,
    json_mode: bool,
    out: Option<PathBuf>,
    /// Write a history record to history.jsonl after each successful upload.
    write_history: bool,
    renderer_opts: pesto::progress::RendererOptions,
}

/// The result of a single upload (one entry in `--each` / `--season`).
struct UploadResult {
    segments: Vec<PostedSegment>,
    groups: Vec<String>,
    cancelled: bool,
    had_failures: bool,
    total_bytes: u64,
    nzb_path: Option<PathBuf>,
}

/// Per-phase wall-clock timing accumulated during a single upload (26g).
#[derive(Default)]
struct PhaseTimings {
    compress_ms: Option<u128>,
    post_ms: Option<u128>,
    check_ms: Option<u128>,
}

/// Run one complete upload: expand `entry_paths`, compress, post, write NZB.
///
/// Returns the posted segments so the caller can build a consolidated season NZB.
async fn run_single_upload(
    params: &UploadParams,
    entry_paths: &[PathBuf],
    entry_label: &str,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<UploadResult> {
    let config = &params.config;
    let upload_start = std::time::Instant::now();
    let mut timings = PhaseTimings::default();

    let mut inputs = pesto::walk::expand_inputs(entry_paths)?;
    let (_file_count, _folder_count, total_bytes) = upload_summary(&inputs);

    // Run pre-hook before anything else (before compression, PAR2, or NNTP).
    // Non-zero exit aborts the upload immediately.
    if !config.no_hooks && !config.dry_run {
        if let Some(cmd) = &config.pre_hook {
            let input_paths_str = inputs
                .iter()
                .map(|f| f.path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(":");
            let pre_env = HookEnv {
                nzb_path: None,
                nfo_path: None,
                name: entry_label,
                total_bytes,
                input_paths: &input_paths_str,
                group: config.groups.first().map(String::as_str),
                password: None,
                server: &config.host,
            };
            run_pre_hook(cmd, &pre_env)?;
        }
    }

    if !params.json_mode && !params.renderer_opts.quiet && std::io::stderr().is_terminal() {
        pesto::progress::print_tree(&inputs);
        let compress_fmt = config.compress_format.as_deref().or_else(|| {
            if config.compress_password.is_some() {
                Some("7z")
            } else {
                None
            }
        });
        pesto::progress::print_upload_flags(&pesto::progress::UploadFlags {
            obfuscate: match config.obfuscate {
                ObfuscateMode::None => "none",
                ObfuscateMode::Full => "full",
                ObfuscateMode::Paranoid => "paranoid",
            },
            compress: compress_fmt,
            password: config.compress_password.as_deref(),
            par2: config.par2,
            resume: config.resume,
            verify: config.verify,
        });
    }

    let (progress_tx, renderer) = if params.json_mode {
        pesto::progress::spawn_json_emitter()
    } else {
        pesto::ui::terminal::spawn_renderer_with(params.renderer_opts.clone())
    };

    // ── Compression ──────────────────────────────────────────────────────────
    let compress_format_str: Option<String> = config.compress_format.clone().or_else(|| {
        if config.compress_password.is_some() {
            Some("7z".to_string())
        } else {
            None
        }
    });
    let effective_password: Option<String> = config.compress_password.clone();

    let compress_temp_dir: Option<PathBuf>;
    if let Some(fmt_str) = &compress_format_str {
        let format = ArchiveFormat::parse(fmt_str).ok_or_else(|| {
            anyhow::anyhow!("unknown compression format `{fmt_str}`; supported: 7z, zip, rar")
        })?;

        if format == ArchiveFormat::Rar && pesto::compress::find_binary("rar").is_none() {
            eprintln!("note: rar password protection requires the `rar` binary in PATH");
        }

        let archive_stem = upload_root(&inputs)
            .or_else(|| {
                inputs.first().map(|f| {
                    PathBuf::from(&f.name)
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                })
            })
            .unwrap_or_else(|| "archive".to_string());

        let archive_stem = if config.obfuscate != ObfuscateMode::None {
            pesto::article::obfuscated_name()
        } else {
            archive_stem
        };

        let tmp_dir = std::env::temp_dir().join(format!(
            "pesto_compress_{}_{}",
            std::process::id(),
            entry_label
        ));
        compress_temp_dir = Some(tmp_dir.clone());

        let fs_paths: Vec<PathBuf> = collect_compress_roots(&inputs);
        let compress_input_bytes: u64 = fs_paths.iter().map(|p| dir_or_file_size(p)).sum();

        let t_compress = std::time::Instant::now();
        let _ = progress_tx.send(pesto::progress::ProgressEvent::CompressStarted {
            total_bytes: compress_input_bytes,
        });

        let archive_path_for_poll =
            tmp_dir.join(format!("{}.{}", archive_stem, format.extension()));
        let poll_tx = progress_tx.clone();
        let poll_path = archive_path_for_poll.clone();
        let poll_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Ok(meta) = tokio::fs::metadata(&poll_path).await {
                    let _ = poll_tx.send(pesto::progress::ProgressEvent::CompressProgress {
                        bytes_written: meta.len(),
                    });
                }
            }
        });

        let compress_inputs = fs_paths.clone();
        let compress_stem = archive_stem.clone();
        let compress_dest = tmp_dir.clone();
        let compress_pass = effective_password.clone();
        let result = tokio::task::spawn_blocking(move || {
            compress(
                &compress_inputs,
                &compress_stem,
                &compress_dest,
                format,
                compress_pass.as_deref(),
            )
        })
        .await
        .context("compressor task panicked")??;

        poll_handle.abort();
        let _ = progress_tx.send(pesto::progress::ProgressEvent::CompressDone);
        let compress_ms = t_compress.elapsed().as_millis();
        info!(elapsed_ms = compress_ms, phase = "compress", "phase done");
        timings.compress_ms = Some(compress_ms);

        let archive_name = result
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        inputs = vec![pesto::walk::InputFile {
            path: result.path,
            name: archive_name,
        }];

        if let Some(pw) = &effective_password {
            let was_auto = params.archive_password_raw.as_deref() == Some("");
            if was_auto {
                println!("archive password: {pw}");
            }
        }
    } else {
        compress_temp_dir = None;
    }
    // ─────────────────────────────────────────────────────────────────────────

    // Derive NZB stem from: --out > nzb_default > nzb_dir/<stem>.nzb > ./<stem>.nzb
    // Always use the original entry_paths for the stem so obfuscation/compression
    // does not leak the randomised archive name into the output filenames.
    //
    // nzb_stem: bare filename without extension, used to name the NZB.
    // nzb_user_dest: optional user-requested destination (--out or nzb_dir).
    //   The canonical copy always goes to ~/.config/pesto/nzb/TIMESTAMP_stem.nzb;
    //   a hardlink (or copy) is placed at nzb_user_dest when set.
    let nzb_stem: Option<String> = params
        .out
        .as_ref()
        .map(|p| {
            p.with_extension("")
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .or_else(|| {
            params.nzb_default.as_deref().map(|s| {
                PathBuf::from(s)
                    .with_extension("")
                    .to_string_lossy()
                    .into_owned()
            })
        })
        .or_else(|| {
            entry_paths
                .first()
                .and_then(|p| {
                    p.file_name().map(|s| {
                        // Release directories use the full folder name as the NZB
                        // stem — calling file_stem() would strip codec tags like
                        // "264" from "H.264" or "0" from "AAC2.0".
                        if p.is_dir() {
                            s.to_string_lossy().into_owned()
                        } else {
                            std::path::Path::new(s)
                                .file_stem()
                                .unwrap_or(s)
                                .to_string_lossy()
                                .into_owned()
                        }
                    })
                })
                .or_else(|| upload_root(&inputs))
                .or_else(|| {
                    inputs.first().map(|f| {
                        let top = f.name.split('/').next().unwrap_or(&f.name);
                        // When the name has a slash, top is a directory component —
                        // use it as-is to avoid stripping codec tags.
                        if f.name.contains('/') {
                            top.to_owned()
                        } else {
                            PathBuf::from(top)
                                .file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        }
                    })
                })
        });

    // User-specified destination directory/path for the NZB hardlink.
    // Priority: --out > nzb_dir > directory next to the uploaded file(s).
    let nzb_user_dest: Option<PathBuf> = params.out.clone().or_else(|| {
        nzb_stem.as_deref().and_then(|stem| {
            if let Some(dir) = config.nzb_dir.as_deref() {
                Some(expand_tilde(dir).join(format!("{stem}.nzb")))
            } else {
                // Default: place the NZB next to the uploaded file/directory.
                entry_paths
                    .first()
                    .and_then(|p| {
                        if p.is_dir() {
                            Some(p.as_path())
                        } else {
                            p.parent()
                        }
                    })
                    .map(|d| d.join(format!("{stem}.nzb")))
            }
        })
    });

    // Resume state is keyed to the user-visible stem so it is stable across re-posts.
    let resume_path: Option<PathBuf> = nzb_user_dest
        .as_ref()
        .map(|p| p.with_extension("pesto-state"))
        .or_else(|| {
            nzb_stem
                .as_deref()
                .map(|s| PathBuf::from(s).with_extension("pesto-state"))
        });

    // nzb_out_path is resolved at write time (after post) — placeholder kept for
    // symmetry with the rest of the function.
    let nzb_out_path: Option<String> = nzb_stem.clone();

    let t_post = std::time::Instant::now();
    let outcome = pesto::poster::post_files_with_progress_and_cancel(
        config,
        &inputs,
        Some(progress_tx),
        resume_path.as_deref(),
        cancel.cloned(),
    )
    .await?;
    let _ = renderer.await;
    timings.post_ms = Some(t_post.elapsed().as_millis());

    // ── Retry segments that failed during the upload ──────────────────────────
    let mut outcome = outcome;
    if !outcome.failed_tasks.is_empty()
        && !config.dry_run
        && !config.par2_only
        && !outcome.cancelled
    {
        let n = outcome.failed_tasks.len();
        eprintln!("{n} segment(s) failed during upload — retrying…");

        let (retry_tx, retry_renderer) = if params.json_mode {
            pesto::progress::spawn_json_emitter()
        } else {
            pesto::ui::terminal::spawn_renderer_with(params.renderer_opts.clone())
        };

        let recovered = pesto::poster::repost_failed_tasks(
            config,
            &outcome.failed_tasks,
            &outcome.groups,
            Some(&retry_tx),
            cancel,
        )
        .await
        .unwrap_or_else(|e| {
            eprintln!("retry: error: {e:#}");
            error!(error = %e, "retry: repost_failed_tasks error");
            Vec::new()
        });

        drop(retry_tx);
        let _ = retry_renderer.await;

        let r = recovered.len();
        eprintln!("retry: {r}/{n} segment(s) recovered");
        outcome.segments.extend(recovered);
        // Remove recovered tasks from the failure lists so they don't appear
        // as failures in the final summary and NZB is written correctly.
        if r == n {
            outcome.failures.retain(|f| {
                !outcome.failed_tasks.iter().any(|t| {
                    f.starts_with(&t.file_name) && f.contains(&format!("{}/{}", t.part, t.total))
                })
            });
            outcome.failed_tasks.clear();
        }
    }

    // ── Post-check STAT pass ──────────────────────────────────────────────────
    let mut cancelled = outcome.cancelled || cancel.is_some_and(|f| f.load(Ordering::Relaxed));
    let check_missing: Vec<String> = if config.check
        && !config.dry_run
        && !config.par2_only
        && !cancelled
        && !outcome.segments.is_empty()
    {
        let (check_tx, check_renderer) = if params.json_mode {
            pesto::progress::spawn_json_emitter()
        } else {
            pesto::ui::terminal::spawn_renderer_with(params.renderer_opts.clone())
        };
        let t_check = std::time::Instant::now();
        let missing =
            pesto::poster::check_articles(config, &outcome.segments, Some(&check_tx), cancel)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("check: error during STAT pass: {e:#}");
                    error!(error = %e, "check: STAT pass failed");
                    Vec::new()
                });
        cancelled = cancelled || cancel.is_some_and(|f| f.load(Ordering::Relaxed));
        drop(check_tx);
        let _ = check_renderer.await;
        let check_ms = t_check.elapsed().as_millis();
        info!(elapsed_ms = check_ms, phase = "check", "phase done");
        timings.check_ms = Some(check_ms);
        missing
    } else {
        Vec::new()
    };

    if !params.json_mode && config.par2_only {
        if cancelled {
            println!("PAR2 generation interrupted.");
        } else {
            println!("PAR2 generation complete.");
        }
    }

    if cancelled {
        if config.par2_only {
            eprintln!("interrupted — stopped before finishing PAR2 generation");
        } else {
            eprintln!("interrupted — upload incomplete");
        }
    }
    if !outcome.failures.is_empty() {
        eprintln!("{} segment(s) failed:", outcome.failures.len());
        for failure in &outcome.failures {
            eprintln!("  - {failure}");
        }
    }
    let check_missing = if !cancelled && !check_missing.is_empty() {
        eprintln!(
            "check: {} article(s) not found — reposting…",
            check_missing.len()
        );
        warn!(
            count = check_missing.len(),
            "check: articles not found on server"
        );

        let (repost_tx, repost_renderer) = if params.json_mode {
            pesto::progress::spawn_json_emitter()
        } else {
            pesto::ui::terminal::spawn_renderer_with(params.renderer_opts.clone())
        };

        let reposted = pesto::poster::repost_missing_segments(
            config,
            &outcome.segments,
            &check_missing,
            Some(&repost_tx),
            cancel,
        )
        .await
        .unwrap_or_else(|e| {
            eprintln!("check: repost error: {e:#}");
            error!(error = %e, "check: repost_missing_segments failed");
            0
        });

        drop(repost_tx);
        let _ = repost_renderer.await;

        eprintln!(
            "check: reposted {reposted}/{} article(s)",
            check_missing.len()
        );

        cancelled = cancelled || cancel.is_some_and(|f| f.load(Ordering::Relaxed));

        // Second STAT pass to confirm reposts landed (no extra delay — they
        // were just posted so propagation should be immediate).
        if cancelled {
            eprintln!("check: interrupted — skipping verify after repost");
            check_missing
        } else {
            let (verify_tx, verify_renderer) = if params.json_mode {
                pesto::progress::spawn_json_emitter()
            } else {
                pesto::ui::terminal::spawn_renderer_with(params.renderer_opts.clone())
            };
            let still_missing =
                pesto::poster::check_articles(config, &outcome.segments, Some(&verify_tx), cancel)
                    .await
                    .unwrap_or_else(|e| {
                        eprintln!("check: verify after repost failed: {e:#}");
                        error!(error = %e, "check: second STAT pass (post-repost verify) failed");
                        check_missing.clone()
                    });
            cancelled = cancelled || cancel.is_some_and(|f| f.load(Ordering::Relaxed));
            drop(verify_tx);
            let _ = verify_renderer.await;

            if cancelled {
                eprintln!("check: interrupted during verify after repost");
                still_missing
            } else if still_missing.is_empty() {
                eprintln!("check: all article(s) confirmed after repost");
                still_missing
            } else {
                eprintln!(
                    "check: {} article(s) still missing after repost:",
                    still_missing.len()
                );
                for id in &still_missing {
                    eprintln!("  - {id}");
                }
                error!(
                    count = still_missing.len(),
                    ids = ?still_missing,
                    "check: articles still missing after repost"
                );
                still_missing
            }
        }
    } else {
        if config.check
            && !config.dry_run
            && !config.par2_only
            && !cancelled
            && !outcome.segments.is_empty()
        {
            eprintln!("check: all {} article(s) verified", outcome.segments.len());
        }
        Vec::new()
    };

    // If segments still failed after retry, refuse to write the NZB — it
    // would be incomplete. The resume state already has all successfully
    // posted segments so the user can continue with --resume.
    let has_unrecoverable_failures =
        !outcome.failed_tasks.is_empty() && !config.dry_run && !config.par2_only;
    if has_unrecoverable_failures {
        let n = outcome.failed_tasks.len();
        eprintln!();
        eprintln!("error: {n} segment(s) could not be posted after all retries.");
        eprintln!("The NZB will NOT be written — the upload is incomplete.");
        if let Some(ref state_path) = resume_path {
            eprintln!();
            eprintln!("The successfully posted segments have been saved to:");
            eprintln!("  {}", state_path.display());
            eprintln!();
            let files_str = entry_paths
                .iter()
                .map(|p| format!("\"{}\"", p.display()))
                .collect::<Vec<_>>()
                .join(" ");
            eprintln!("To retry the missing segments and finish the upload, run:");
            eprintln!("  pesto {files_str} --resume");
        }
        eprintln!();
    }

    // Write NZB.
    // The canonical copy goes to ~/.config/pesto/nzb/TIMESTAMP_stem.nzb.
    // If the user specified a destination (--out or nzb_dir), a hardlink (or
    // copy when cross-device) is placed there so re-uploads never collide.
    let out: Option<PathBuf> = if let Some(stem) = nzb_out_path {
        if !cancelled || config.resume {
            Some(nzb_archive_path(&stem).await)
        } else {
            eprintln!("interrupted — skipping nzb output");
            None
        }
    } else {
        None
    };

    // nzb_reported_path: the path shown to the user and passed to hooks/history.
    // It is the user-dest (hardlink) when set, otherwise the archive copy.
    let mut nzb_reported_path: Option<PathBuf> = if cancelled && !config.resume {
        None
    } else {
        nzb_user_dest.clone().or_else(|| out.clone())
    };

    let _nzb_xml: Option<String> = if let Some(out) = &out {
        if !config.par2_only {
            if has_unrecoverable_failures {
                eprintln!("skipping nzb output — upload incomplete");
                nzb_reported_path = None;
                None
            } else if outcome.segments.is_empty() {
                eprintln!("no segments posted — skipping nzb output");
                nzb_reported_path = None;
                None
            } else {
                let nzb_meta = NzbMeta {
                    name: config.nzb_name.clone(),
                    password: config
                        .nzb_password
                        .clone()
                        .or_else(|| effective_password.clone()),
                    category: config.nzb_category.clone(),
                    tags: config.nzb_tags.clone(),
                };
                let xml = pesto::nzb::generate(&outcome.groups, &outcome.segments, &nzb_meta);
                tokio::fs::write(out, &xml)
                    .await
                    .with_context(|| format!("writing nzb file `{}`", out.display()))?;

                // Place a hardlink (or copy) at the user-requested destination,
                // respecting the nzb_conflict policy.
                if let Some(dest) = &nzb_user_dest {
                    if let Some(parent) = dest.parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }
                    let effective_dest = resolve_nzb_dest(dest, config.nzb_conflict).await?;
                    if std::fs::hard_link(out, &effective_dest).is_err() {
                        std::fs::copy(out, &effective_dest).with_context(|| {
                            format!("copying nzb to `{}`", effective_dest.display())
                        })?;
                    }
                    nzb_reported_path = Some(effective_dest);
                }

                let reported = nzb_reported_path.as_deref().unwrap_or(out);
                if params.json_mode {
                    let path_esc = reported
                        .display()
                        .to_string()
                        .replace('\\', "\\\\")
                        .replace('"', "\\\"");
                    println!(r#"{{"type":"nzb_written","path":"{path_esc}"}}"#);
                } else {
                    println!("wrote nzb: {}", reported.display());
                }

                // Append to shared history catalog.
                if params.write_history && !config.par2_only && !config.dry_run {
                    let obf_name = if config.obfuscate != pesto::config::ObfuscateMode::None {
                        Some(entry_label)
                    } else {
                        None
                    };
                    let par2_str;
                    let par2_pct = if config.par2 > 0 {
                        par2_str = format!("{}%", config.par2);
                        Some(par2_str.as_str())
                    } else {
                        None
                    };
                    pesto::history::record_upload(
                        &pesto::history::UploadRecord {
                            name: entry_label,
                            obfuscated_name: obf_name,
                            password: effective_password.as_deref(),
                            total_bytes,
                            group: config.groups.first().map(String::as_str),
                            server: Some(config.host.as_str()),
                            par2_redundancy: par2_pct,
                            duration_secs: upload_start.elapsed().as_secs_f64(),
                            nzb_path: Some(&reported.display().to_string()),
                            subject: config.nzb_name.as_deref().or(Some(entry_label)),
                        },
                        config.history_dir.as_deref(),
                    );
                }

                Some(xml)
            }
        } else {
            None
        }
    } else {
        None
    };

    // Send completion notifications.
    let notify_enabled = config.notify.unwrap_or(true)
        && (config.notify_webhook.is_some() || config.notify_ntfy.is_some());
    if notify_enabled && !config.par2_only && !config.dry_run && !cancelled {
        let had_failures = !outcome.failures.is_empty() || has_unrecoverable_failures;
        pesto::notify::send_all(&pesto::notify::NotifyConfig {
            webhook_url: config.notify_webhook.as_deref(),
            ntfy_topic: config.notify_ntfy.as_deref(),
            name: entry_label,
            total_bytes,
            group: config.groups.first().map(String::as_str),
            category: config.nzb_category.as_deref(),
            ok: !had_failures,
        })
        .await;
    }

    // Generate .nfo as a local artifact only when the upload actually
    // succeeded. Writing it on failure leaves an orphan `.nfo` in the input
    // directory (no nzb_reported_path → fallback next to the source files),
    // which `--resume --each` would later pick up as a standalone release.
    let upload_ok = !cancelled && outcome.failures.is_empty() && !has_unrecoverable_failures;
    let nfo_path: Option<PathBuf> = if config.nfo && upload_ok && !config.par2_only {
        let base = nzb_reported_path
            .as_ref()
            .map(|p| p.with_extension("nfo"))
            .or_else(|| {
                entry_paths
                    .first()
                    .and_then(|p| p.parent())
                    .map(|d| d.join(format!("{entry_label}.nfo")))
            });
        if let Some(ref nfo_out) = base {
            match pesto::nfo::generate(entry_paths) {
                Some(content) => match pesto::nfo::write(nfo_out, &content) {
                    Ok(()) => {
                        println!("wrote nfo:  {}", nfo_out.display());
                        Some(nfo_out.clone())
                    }
                    Err(e) => {
                        eprintln!("nfo write failed: {e}");
                        None
                    }
                },
                None => {
                    eprintln!("nfo: no content generated for the given paths");
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // Run post-upload hooks only when the upload actually succeeded.
    if upload_ok && !config.par2_only && !config.dry_run {
        let post_input_paths = inputs
            .iter()
            .map(|f| f.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let hook_env = HookEnv {
            nzb_path: nzb_reported_path.as_deref(),
            nfo_path: nfo_path.as_deref(),
            name: entry_label,
            total_bytes,
            input_paths: &post_input_paths,
            group: config.groups.first().map(String::as_str),
            password: effective_password.as_deref(),
            server: &config.host,
        };

        if !config.no_hooks {
            // Run --post-hook command.
            if let Some(cmd) = &config.post_hook {
                run_post_hook(cmd, &hook_env);
            }

            // Run every executable script found in ~/.config/pesto/hooks/.
            if let Some(hooks_dir) = pesto::config::config_dir().map(|d| d.join("hooks")) {
                run_hooks_dir(&hooks_dir, &hook_env);
            }
        }
    }

    // Cleanup temp dirs.
    if let Some(dir) = compress_temp_dir {
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 26g — per-phase timing summary (only when -v is active)
    if tracing::enabled!(tracing::Level::INFO) {
        let total_ms = upload_start.elapsed().as_millis();
        let mut parts = Vec::<String>::new();
        if let Some(ms) = timings.compress_ms {
            parts.push(format!("compress={ms}ms"));
        }
        if let Some(ms) = timings.post_ms {
            parts.push(format!("post={ms}ms"));
        }
        if let Some(ms) = timings.check_ms {
            parts.push(format!("check={ms}ms"));
        }
        info!(
            total_ms,
            phases = %parts.join(" "),
            "upload timing summary"
        );
    }

    Ok(UploadResult {
        segments: outcome.segments,
        groups: outcome.groups,
        cancelled,
        had_failures: !outcome.failures.is_empty()
            || !check_missing.is_empty()
            || has_unrecoverable_failures,
        total_bytes,
        nzb_path: nzb_reported_path,
    })
}

/// Whether a top-level entry is a pesto-generated artifact that must never be
/// treated as an independent `--each` release. A bare `.nfo`/`.nzb` sitting in
/// the input directory is one of our own outputs (e.g. an orphan `.nfo` left by
/// a failed run); uploading it as a standalone release is never intended.
fn is_artifact_entry(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| ext == "nfo" || ext == "nzb")
}

/// Enumerate top-level entries of `dir` (files and subdirectories), sorted by
/// name using natural lexical ordering (so `E02` comes before `E10`).
fn top_level_entries(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory `{}`", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| !is_artifact_entry(p))
        .collect();
    entries.sort_by(|a, b| {
        lexical_sort::natural_lexical_cmp(&a.to_string_lossy(), &b.to_string_lossy())
    });
    Ok(entries)
}

/// Run `--each` / `--season` batch over all top-level entries of the given directories.
///
/// Returns all collected segments (for season NZB consolidation) and whether
/// any upload was cancelled or had failures.
async fn run_batch(
    params: Arc<UploadParams>,
    dirs: &[PathBuf],
    jobs: usize,
    season_nzb: Option<PathBuf>,
    cancel: Arc<AtomicBool>,
) -> Result<(Vec<PostedSegment>, bool, bool)> {
    // Collect all entries from every directory argument.
    let mut entries: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let md = std::fs::metadata(dir).with_context(|| format!("reading `{}`", dir.display()))?;
        if md.is_dir() {
            entries.extend(top_level_entries(dir)?);
        } else {
            // A plain file is its own "entry".
            entries.push(dir.clone());
        }
    }

    if entries.is_empty() {
        anyhow::bail!("no entries found to post");
    }

    let effective_jobs = if jobs == 0 {
        parmesan::performance_core_count()
    } else {
        jobs
    };

    let semaphore = Arc::new(tokio::sync::Semaphore::new(effective_jobs));
    let mut all_segments: Vec<PostedSegment> = Vec::new();
    let mut all_groups: Vec<String> = Vec::new();
    let mut any_cancelled = false;
    let mut any_failures = false;

    let mut handles = Vec::new();
    for entry in &entries {
        // Acquire the permit before spawning so uploads start in the sorted
        // order. With the permit inside the task, the scheduler decided which
        // upload ran first, making --each non-deterministic.
        let permit = Arc::clone(&semaphore)
            .acquire_owned()
            .await
            .expect("semaphore closed");
        let entry = entry.clone();
        let params = Arc::clone(&params);
        let task_cancel = cancel.clone();
        let label = entry
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "entry".to_string());

        let handle = tokio::spawn(async move {
            let _permit = permit;
            if !params.json_mode {
                println!("\n── {} ──", label);
            }
            run_single_upload(&params, &[entry], &label, Some(&task_cancel)).await
        });
        handles.push(handle);
    }

    for handle in handles {
        match handle.await {
            Ok(Ok(result)) => {
                all_segments.extend(result.segments);
                for g in result.groups {
                    if !all_groups.contains(&g) {
                        all_groups.push(g);
                    }
                }
                if result.cancelled {
                    any_cancelled = true;
                }
                if result.had_failures {
                    any_failures = true;
                }
            }
            Ok(Err(e)) => {
                eprintln!("upload error: {e:#}");
                any_failures = true;
            }
            Err(e) => {
                eprintln!("upload task panicked: {e}");
                any_failures = true;
            }
        }
    }

    // Write consolidated season NZB (and matching .nfo + hooks) when requested.
    if let Some(season_path) = season_nzb {
        if any_cancelled {
            eprintln!("interrupted — skipping season nzb output");
        } else if !all_segments.is_empty() {
            let config = &params.config;
            let season_name = config.nzb_name.clone().or_else(|| {
                season_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            });
            let nzb_meta = NzbMeta {
                name: season_name,
                password: config
                    .nzb_password
                    .clone()
                    .or_else(|| config.compress_password.clone()),
                category: config.nzb_category.clone(),
                tags: config.nzb_tags.clone(),
            };
            let xml = pesto::nzb::generate(&all_groups, &all_segments, &nzb_meta);
            tokio::fs::write(&season_path, &xml)
                .await
                .with_context(|| format!("writing season nzb `{}`", season_path.display()))?;
            if !params.json_mode {
                println!("\nwrote season nzb: {}", season_path.display());
            } else {
                let path_esc = season_path
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"");
                println!(r#"{{"type":"nzb_written","path":"{path_esc}","season":true}}"#);
            }

            // Generate season .nfo (mediainfo of first episode) next to the NZB.
            let nfo_path: Option<PathBuf> = if config.nfo {
                let nfo_out = season_path.with_extension("nfo");
                match pesto::nfo::generate_season(dirs) {
                    Some(content) => match pesto::nfo::write(&nfo_out, &content) {
                        Ok(()) => {
                            println!("wrote nfo:  {}", nfo_out.display());
                            Some(nfo_out)
                        }
                        Err(e) => {
                            eprintln!("season nfo write failed: {e}");
                            None
                        }
                    },
                    None => None,
                }
            } else {
                None
            };

            // Run post-upload hooks — same as a regular upload.
            let season_label = season_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "season".to_string());
            let total_bytes: u64 = all_segments.iter().map(|s| s.bytes).sum();
            let effective_password = config
                .nzb_password
                .clone()
                .or_else(|| config.compress_password.clone());
            let hook_env = HookEnv {
                nzb_path: Some(&season_path),
                nfo_path: nfo_path.as_deref(),
                name: &season_label,
                total_bytes,
                input_paths: "",
                group: config.groups.first().map(String::as_str),
                password: effective_password.as_deref(),
                server: &config.host,
            };
            // Skip hooks for --dry-run / --par2-only, matching the per-entry
            // path: no real upload happened, so post-upload hooks must not fire.
            if !config.no_hooks && !config.dry_run && !config.par2_only {
                if let Some(cmd) = &config.post_hook {
                    run_post_hook(cmd, &hook_env);
                }
                if let Some(hooks_dir) = pesto::config::config_dir().map(|d| d.join("hooks")) {
                    run_hooks_dir(&hooks_dir, &hook_env);
                }
            }
        }
    }

    Ok((all_segments, any_cancelled, any_failures))
}

/// How many consecutive failed attempts before giving up on an entry.
const WATCH_MAX_RETRIES: u32 = 3;

/// Recursively sum the byte size of a path (file or directory).
fn entry_size(path: &Path) -> u64 {
    if let Ok(md) = std::fs::metadata(path) {
        if md.is_file() {
            return md.len();
        }
    }
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    rd.filter_map(|e| e.ok())
        .map(|e| entry_size(&e.path()))
        .sum()
}

/// Run `--watch DIR`: poll for new entries and post each one automatically.
///
/// New entries are held in a "pending" state until their total byte size is
/// stable across two consecutive polls (settle check), preventing premature
/// uploads of directories that are still being populated.  Failed uploads are
/// retried up to `WATCH_MAX_RETRIES` times before being abandoned.
///
/// Exits cleanly on SIGTERM or Ctrl-C after finishing any in-progress upload.
async fn run_watch(
    params: Arc<UploadParams>,
    watch_dir: &Path,
    watch_done: Option<&Path>,
    poll_interval: u64,
    jobs: usize,
    cancel: Arc<AtomicBool>,
) -> Result<bool> {
    use tokio::sync::mpsc;

    eprintln!(
        "watching {} (poll every {}s)",
        watch_dir.display(),
        poll_interval
    );

    // `done`: entries that have been successfully uploaded (or permanently failed).
    let mut done: HashSet<PathBuf> = HashSet::new();
    // Pre-populate done with whatever is already present so we don't re-post on startup.
    if let Ok(existing) = top_level_entries(watch_dir) {
        for e in existing {
            done.insert(e);
        }
    }

    // `pending`: entries seen but not yet stable.  Value is the size snapshot
    // from the previous poll; once two consecutive polls agree the entry is
    // dispatched for upload.
    let mut pending: HashMap<PathBuf, u64> = HashMap::new();

    // `retry_counts`: number of failed attempts per entry.
    let mut retry_counts: HashMap<PathBuf, u32> = HashMap::new();

    let effective_jobs = if jobs == 0 {
        parmesan::performance_core_count()
    } else {
        jobs
    };
    let semaphore = Arc::new(tokio::sync::Semaphore::new(effective_jobs));

    // Channel for completed tasks to report back (path, success, cancelled).
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<(PathBuf, bool, bool)>();

    let mut any_cancelled = false;

    loop {
        // Check for shutdown between polls.
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(poll_interval)) => {}
            _ = async {
                while !cancel.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            } => {
                eprintln!("\nshutdown requested — finishing in-progress uploads");
                break;
            }
        }

        // Drain completed-task notifications before scanning for new entries.
        while let Ok((entry, success, task_cancelled)) = result_rx.try_recv() {
            if task_cancelled {
                any_cancelled = true;
                eprintln!("watch: upload of `{}` was cancelled", entry.display());
            } else if success {
                done.insert(entry);
            } else {
                let attempts = retry_counts.entry(entry.clone()).or_insert(0);
                *attempts += 1;
                if *attempts >= WATCH_MAX_RETRIES {
                    eprintln!(
                        "watch: giving up on `{}` after {WATCH_MAX_RETRIES} failed attempts",
                        entry.display()
                    );
                    done.insert(entry);
                } else {
                    eprintln!(
                        "watch: will retry `{}` (attempt {}/{})",
                        entry.display(),
                        attempts,
                        WATCH_MAX_RETRIES
                    );
                    // Remove from pending so it goes through the settle check again.
                    pending.remove(&entry);
                }
            }
        }

        let entries = match top_level_entries(watch_dir) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("watch: error reading {}: {e}", watch_dir.display());
                continue;
            }
        };

        for entry in entries {
            if done.contains(&entry) {
                continue;
            }

            let current_size = entry_size(&entry);

            match pending.get(&entry).copied() {
                None => {
                    // First time we see this entry — record its size and wait.
                    pending.insert(entry.clone(), current_size);
                    let label = entry
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "entry".to_string());
                    eprintln!("watch: detected `{label}` — waiting for it to stabilise");
                }
                Some(prev_size) if prev_size != current_size => {
                    // Still changing — update snapshot and keep waiting.
                    pending.insert(entry.clone(), current_size);
                }
                Some(_) => {
                    // Size unchanged since last poll: entry is stable, dispatch it.
                    pending.remove(&entry);
                    // Acquire the permit before spawning so uploads start in the
                    // sorted order returned by top_level_entries().
                    let permit = Arc::clone(&semaphore)
                        .acquire_owned()
                        .await
                        .expect("semaphore closed");
                    // Mark as done immediately so a second poll won't re-queue it
                    // while the upload task holds the semaphore permit.
                    done.insert(entry.clone());

                    let params = Arc::clone(&params);
                    let watch_done = watch_done.map(PathBuf::from);
                    let tx = result_tx.clone();
                    let label = entry
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "entry".to_string());
                    let task_cancel = cancel.clone();

                    tokio::spawn(async move {
                        let _permit = permit;
                        if !params.json_mode {
                            println!("\n── watch: {} ──", label);
                        }
                        let (success, task_cancelled) = match run_single_upload(
                            &params,
                            std::slice::from_ref(&entry),
                            &label,
                            Some(&task_cancel),
                        )
                        .await
                        {
                            Ok(result) if result.cancelled => (false, true),
                            Ok(_) => {
                                // Move to --watch-done if specified; otherwise leave in place.
                                if let Some(done_dir) = &watch_done {
                                    let dest = done_dir.join(entry.file_name().unwrap_or_default());
                                    if let Err(e) = std::fs::rename(&entry, &dest) {
                                        eprintln!(
                                            "watch: could not move `{}` to `{}`: {e}",
                                            entry.display(),
                                            dest.display()
                                        );
                                    }
                                }
                                (true, false)
                            }
                            Err(e) => {
                                eprintln!("watch: upload failed for `{}`: {e:#}", entry.display());
                                (false, false)
                            }
                        };
                        // Report outcome; if the channel is closed we're shutting down.
                        let _ = tx.send((entry, success, task_cancelled));
                    });
                }
            }
        }
    }

    // Wait for all in-progress uploads (drain the semaphore).
    let effective_jobs = if jobs == 0 {
        parmesan::performance_core_count()
    } else {
        jobs
    };
    let _ = semaphore.acquire_many(effective_jobs as u32).await;
    eprintln!("watch: all uploads finished, exiting");
    Ok(any_cancelled)
}

// ── merge-season ─────────────────────────────────────────────────────────────

/// Group all `.nzb` files in `dir` by season, merge each group into one
/// combined NZB, and write it beside the source files.
fn run_merge_season(dir: &Path, display_name: Option<&str>) -> Result<()> {
    use std::collections::BTreeMap;

    anyhow::ensure!(dir.is_dir(), "{} is not a directory", dir.display());

    // Collect .nzb files, sorted so episodes come out in order.
    let mut nzb_files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("nzb"))
        .collect();
    nzb_files.sort();

    anyhow::ensure!(
        !nzb_files.is_empty(),
        "no .nzb files found in {}",
        dir.display()
    );

    // Group files by season key.  A season key is the show name plus the
    // season number extracted from the filename, e.g. "Batwheels.S02".
    // Files with no recognisable season marker fall into a catch-all group
    // named after the directory.
    let fallback_key = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "season".into());

    let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for path in &nzb_files {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let key = season_key(&stem).unwrap_or_else(|| fallback_key.clone());
        groups.entry(key).or_default().push(path.clone());
    }

    for (key, files) in &groups {
        // Skip if only one file in the group — nothing to merge.
        // (Single-file "seasons" are already complete NZBs.)
        if files.len() < 2 {
            eprintln!("skipping {key}: only one NZB in group");
            continue;
        }

        let output_path = dir.join(format!("{key}.nzb"));

        // Don't include the output file itself if it already exists in `files`.
        let sources: Vec<&PathBuf> = files
            .iter()
            .filter(|p| p.as_path() != output_path.as_path())
            .collect();

        eprintln!(
            "\nmerging {} episodes into {}",
            sources.len(),
            output_path.display()
        );

        let mut combined_segments: Vec<pesto::poster::PostedSegment> = Vec::new();
        let mut poster = String::new();
        let mut all_groups: Vec<String> = Vec::new();

        for src in &sources {
            let content = std::fs::read_to_string(src)
                .with_context(|| format!("reading {}", src.display()))?;
            let parsed = pesto::nzb::parse(&content)
                .with_context(|| format!("parsing {}", src.display()))?;

            let ep_name = src
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| src.display().to_string());
            let file_count = parsed
                .segments
                .iter()
                .map(|s| &s.file_name)
                .collect::<std::collections::HashSet<_>>()
                .len();
            let seg_count = parsed.segments.len();
            eprintln!("  + {ep_name}  ({file_count} file(s), {seg_count} segment(s))");

            if poster.is_empty() {
                poster = parsed.poster;
            }
            for g in parsed.groups {
                if !all_groups.contains(&g) {
                    all_groups.push(g);
                }
            }
            combined_segments.extend(parsed.segments);
        }

        combined_segments.sort_by(|a, b| a.file_name.cmp(&b.file_name).then(a.part.cmp(&b.part)));

        let meta = pesto::nzb::NzbMeta {
            name: display_name
                .map(str::to_string)
                .or_else(|| Some(key.clone())),
            password: None,
            category: None,
            tags: Vec::new(),
        };
        let xml = pesto::nzb::generate(&all_groups, &combined_segments, &meta);

        std::fs::write(&output_path, &xml)
            .with_context(|| format!("writing {}", output_path.display()))?;

        eprintln!(
            "wrote {} ({} total segments)",
            output_path.display(),
            combined_segments.len()
        );
    }

    Ok(())
}

/// Extract a season group key from an NZB stem.
///
/// `Batwheels.S02E32-E33.1080p.NF.WEB-DL` → `Batwheels.S02`
/// `Show.Name.s01e01.720p`                  → `Show.Name.S01`
/// `Random.File`                            → `None`
fn season_key(stem: &str) -> Option<String> {
    let lower = stem.to_lowercase();
    let bytes = lower.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b's' {
            // Require at least one digit after 's'.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j == i + 1 {
                continue; // no digits after 's'
            }
            // Require 'e' followed by at least one digit.
            if j < bytes.len()
                && bytes[j] == b'e'
                && j + 1 < bytes.len()
                && bytes[j + 1].is_ascii_digit()
            {
                // stem[..j] covers everything up to 'e', including 'SXX'.
                // Reconstruct with original case up to the 's', then uppercase season.
                let prefix = &stem[..i];
                let season_num = &stem[i + 1..j]; // digits only
                return Some(format!(
                    "{prefix}S{:0>2}",
                    season_num.parse::<u32>().unwrap_or(0)
                ));
            }
        }
    }
    None
}

/// Append a one-line structured summary to the session log file.
///
/// Written after the upload completes so it is always the last line, making
/// `tail -1` a reliable way to check the outcome of any upload.
fn write_session_summary(
    path: &Path,
    label: &str,
    cancelled: bool,
    had_failures: bool,
    total_bytes: u64,
    nzb_path: Option<&Path>,
) {
    use std::io::Write;

    let status = if cancelled {
        "cancelled"
    } else if had_failures {
        "failed"
    } else {
        "ok"
    };

    let total_mb = total_bytes as f64 / 1_048_576.0;
    let nzb = nzb_path
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("-");

    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%SZ");
    let line = format!(
        "{now}  summary  status={status}  label=\"{label}\"  bytes={total_mb:.1}MiB  nzb={nzb}\n"
    );

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();

    // `pesto --config` with no value: launch the interactive setup wizard.
    if matches!(cli.config, Some(None)) {
        return pesto::ui::wizard::run();
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

    // --merge-season: offline NZB merge, no server connection needed.
    if let Some(ref dir) = cli.merge_season {
        // No upload here, so no session log — just honour -v/--log-file.
        logging::init(cli.verbose, cli.log_file.as_deref(), None)?;
        return run_merge_season(dir, cli.nzb_name.as_deref());
    }

    // `pesto` with nothing to post and no --watch: show the orientation screen.
    let has_work = !cli.files.is_empty() || cli.watch.is_some();
    if !has_work {
        print_welcome();
        return Ok(());
    }

    print_header();

    // Resolve config file.
    let (file_config, nzb_default) = match &cli.config {
        Some(Some(path)) => (FileConfig::load(path)?, None),
        _ => match config::default_config_path().filter(|p| p.exists()) {
            Some(path) => {
                eprintln!("using config: {}", path.display());
                let fc = FileConfig::load(&path)?;
                let nzb = fc.output.nzb.clone();
                (fc, nzb)
            }
            None => (FileConfig::default(), None),
        },
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
    if let Some(p) = &session_log {
        tracing::debug!(path = %p.display(), "session log");
    }

    // Suppress the terminal panel when debug-level logs are going to stderr to
    // avoid the panel and log lines corrupting each other. If the user redirected
    // logs to a file with --log-file the panel can run alongside safely.
    let logs_to_stderr = cli.verbose >= 2 && cli.log_file.is_none();

    let params = Arc::new(UploadParams {
        config: Arc::clone(&config),
        archive_password_raw: cli.archive_password.clone(),
        nzb_default: nzb_default.map(|s| s.to_string()),
        json_mode,
        out: cli.out.clone(),
        write_history: config.history,
        renderer_opts: pesto::progress::RendererOptions {
            quiet: cli.quiet || config.quiet || logs_to_stderr,
            bell: cli.bell || config.bell,
        },
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
                cli.files.iter().find_map(|p| {
                    let md = std::fs::metadata(p).ok()?;
                    if md.is_dir() {
                        let name = p.file_name()?.to_string_lossy();
                        let stem = format!("{name}.nzb");
                        let path = if let Some(dir) = &params.config.nzb_dir {
                            expand_tilde(dir).join(&stem)
                        } else {
                            PathBuf::from(&stem)
                        };
                        Some(path)
                    } else {
                        None
                    }
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
    // Derive a human-readable label from the first input path without any
    // blocking filesystem calls (no is_dir/stat in the async executor).
    // file_name() returns the last path component; we strip a known extension
    // with file_stem() only when the OsStr round-trip gives us one.
    let label = cli
        .files
        .first()
        .and_then(|p| p.file_name())
        .map(|s| {
            let p = std::path::Path::new(s);
            p.file_stem().unwrap_or(s).to_string_lossy().into_owned()
        })
        .unwrap_or_else(|| format!("{}", std::process::id()));
    let result = run_single_upload(&params, &cli.files, &label, Some(&cancel)).await?;

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

/// Collect the unique filesystem paths to pass to the compressor.
fn collect_compress_roots(inputs: &[pesto::walk::InputFile]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for input in inputs {
        let depth = input.name.split('/').count();
        let root = if depth <= 1 {
            input.path.clone()
        } else {
            input
                .path
                .ancestors()
                .nth(depth)
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| input.path.clone())
        };
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    if roots.is_empty() {
        inputs.iter().map(|f| f.path.clone()).collect()
    } else {
        roots
    }
}

/// The single root folder shared by every input, or `None` for loose files.
fn upload_root(inputs: &[pesto::walk::InputFile]) -> Option<String> {
    let mut root: Option<&str> = None;
    for input in inputs {
        let (candidate, _) = input.name.split_once('/')?;
        match root {
            Some(existing) if existing != candidate => return None,
            _ => root = Some(candidate),
        }
    }
    root.map(str::to_string)
}

/// Recursively sum bytes for a path that may be a file or a directory.
fn dir_or_file_size(path: &Path) -> u64 {
    match std::fs::metadata(path) {
        Err(_) => 0,
        Ok(m) if m.is_file() => m.len(),
        Ok(_) => {
            let mut total = 0u64;
            if let Ok(rd) = std::fs::read_dir(path) {
                for entry in rd.flatten() {
                    total += dir_or_file_size(&entry.path());
                }
            }
            total
        }
    }
}

/// Aggregate the upload as `(file count, subfolder count, total bytes)`.
fn upload_summary(inputs: &[pesto::walk::InputFile]) -> (usize, usize, u64) {
    let mut subfolders = std::collections::BTreeSet::new();
    let mut bytes = 0u64;
    for input in inputs {
        let components: Vec<&str> = input.name.split('/').collect();
        let mut prefix = String::new();
        for component in &components[..components.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if prefix.contains('/') {
                subfolders.insert(prefix.clone());
            }
        }
        if let Ok(metadata) = std::fs::metadata(&input.path) {
            bytes += metadata.len();
        }
    }
    (inputs.len(), subfolders.len(), bytes)
}

/// Print the orientation screen shown when `pesto` is run with no files.
fn print_header() {
    eprintln!(
        "pesto v{} — fast, lean Usenet poster",
        env!("CARGO_PKG_VERSION")
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

struct HookEnv<'a> {
    nzb_path: Option<&'a std::path::Path>,
    nfo_path: Option<&'a std::path::Path>,
    name: &'a str,
    total_bytes: u64,
    /// Colon-separated list of input paths (empty string when unknown).
    input_paths: &'a str,
    group: Option<&'a str>,
    password: Option<&'a str>,
    server: &'a str,
}

fn apply_hook_env(child: &mut std::process::Command, env: &HookEnv<'_>) {
    child.env("PESTO_NAME", env.name);
    child.env("PESTO_BYTES", env.total_bytes.to_string());
    child.env("PESTO_INPUT_PATHS", env.input_paths);
    child.env("PESTO_SERVER", env.server);
    child.env("PESTO_GROUP", env.group.unwrap_or(""));
    child.env("PESTO_PASSWORD", env.password.unwrap_or(""));
    child.env(
        "PESTO_NZB",
        env.nzb_path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    child.env(
        "PESTO_NFO",
        env.nfo_path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
}

/// Execute a shell command as a pre-upload hook.
///
/// Runs via `sh -c` on Unix and `cmd /c` on Windows. Returns `Ok(())` when
/// the command exits with status 0, or an error (which aborts the upload) on
/// non-zero exit or if the process could not be started.
fn run_pre_hook(cmd: &str, env: &HookEnv<'_>) -> Result<()> {
    #[cfg(unix)]
    let mut child = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    #[cfg(windows)]
    let mut child = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", cmd]);
        c
    };
    apply_hook_env(&mut child, env);
    match child.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => anyhow::bail!("pre-hook exited with status {s} — upload aborted"),
        Err(e) => anyhow::bail!("pre-hook failed to start: {e} — upload aborted"),
    }
}

/// Execute a shell command as a post-upload hook.
///
/// Runs via `sh -c` on Unix and `cmd /c` on Windows so any interpreter works.
/// Errors are logged but never abort the caller.
fn run_post_hook(cmd: &str, env: &HookEnv<'_>) {
    #[cfg(unix)]
    let mut child = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    #[cfg(windows)]
    let mut child = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", cmd]);
        c
    };
    apply_hook_env(&mut child, env);
    match child.status() {
        Ok(s) if s.success() => println!("post-hook exited ok"),
        Ok(s) => eprintln!("post-hook exited with status {s}"),
        Err(e) => eprintln!("post-hook failed to start: {e}"),
    }
}

/// Run every executable file in `hooks_dir`, sorted by name.
///
/// Each script is executed directly (not via a shell) so it must have a
/// shebang line on Unix or a registered extension on Windows. Errors per
/// script are logged individually; one failing hook does not skip the rest.
fn run_hooks_dir(hooks_dir: &std::path::Path, env: &HookEnv<'_>) {
    let Ok(entries) = std::fs::read_dir(hooks_dir) else {
        return;
    };
    let mut scripts: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_executable(p))
        .collect();
    scripts.sort();
    for script in &scripts {
        println!("running hook: {}", script.display());
        let mut child = std::process::Command::new(script);
        apply_hook_env(&mut child, env);
        match child.status() {
            Ok(s) if s.success() => println!("  hook exited ok"),
            Ok(s) => eprintln!("  hook exited with status {s}"),
            Err(e) => eprintln!("  hook failed to start: {e}"),
        }
    }
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("exe" | "cmd" | "bat" | "ps1" | "py")
    )
}

/// Return a unique path for the NZB using `O_CREAT|O_EXCL` (atomic create).
///
/// Tries `base.nzb`, then `base.v2.nzb`, `base.v3.nzb`, … until it can
/// Resolve the final user-destination path for the NZB according to the
/// conflict policy. Returns an error when the policy is `Fail` and the file
/// already exists.
async fn resolve_nzb_dest(
    dest: &Path,
    conflict: pesto::config::NzbConflict,
) -> anyhow::Result<PathBuf> {
    use pesto::config::NzbConflict;
    if !dest.exists() {
        return Ok(dest.to_path_buf());
    }
    match conflict {
        NzbConflict::Overwrite => Ok(dest.to_path_buf()),
        NzbConflict::Rename => {
            let base = dest.with_extension("");
            let stem = base.to_string_lossy();
            let mut n = 1u32;
            loop {
                let candidate = PathBuf::from(format!("{stem}-{n}.nzb"));
                if !candidate.exists() {
                    return Ok(candidate);
                }
                n += 1;
            }
        }
        NzbConflict::Fail => {
            anyhow::bail!(
                "nzb file already exists: {} (set nzb_conflict = \"overwrite\" or \"rename\" to allow)",
                dest.display()
            )
        }
    }
}

/// Return the canonical NZB archive path: `~/.config/pesto/nzb/TIMESTAMP_stem.nzb`.
/// Creates the directory if needed. The timestamp prefix makes every upload
/// unique so overwrites are never an issue.
async fn nzb_archive_path(stem: &str) -> PathBuf {
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let filename = format!("{timestamp}_{stem}.nzb");

    if let Some(dir) = pesto::config::config_dir().map(|d| d.join("nzb")) {
        let _ = tokio::fs::create_dir_all(&dir).await;
        dir.join(filename)
    } else {
        PathBuf::from(filename)
    }
}

/// Expand a leading `~` to the user's home directory.
/// Returns the path unchanged when `~` is not present or `$HOME` is unset.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pesto::walk::InputFile;

    fn inputs(names: &[&str]) -> Vec<InputFile> {
        names
            .iter()
            .map(|n| InputFile {
                path: PathBuf::from(n),
                name: n.to_string(),
            })
            .collect()
    }

    #[test]
    fn upload_root_finds_a_single_shared_directory() {
        assert_eq!(
            upload_root(&inputs(&["Show/ep01.bin", "Show/extras/clip.bin"])),
            Some("Show".to_string())
        );
    }

    #[test]
    fn upload_root_is_none_for_loose_or_mixed_inputs() {
        assert_eq!(upload_root(&inputs(&["a.bin"])), None);
        assert_eq!(upload_root(&inputs(&["A/x.bin", "B/y.bin"])), None);
        assert_eq!(upload_root(&inputs(&["Show/ep01.bin", "loose.bin"])), None);
    }

    #[test]
    fn collect_compress_roots_loose_file_is_the_file_itself() {
        let files = vec![InputFile {
            path: PathBuf::from("/media/downloads/movie.mkv"),
            name: "movie.mkv".to_string(),
        }];
        assert_eq!(
            collect_compress_roots(&files),
            vec![PathBuf::from("/media/downloads/movie.mkv")]
        );
    }

    #[test]
    fn collect_compress_roots_directory_input_strips_correctly() {
        let files = vec![
            InputFile {
                path: PathBuf::from("/media/Show/ep01.mkv"),
                name: "Show/ep01.mkv".to_string(),
            },
            InputFile {
                path: PathBuf::from("/media/Show/ep02.mkv"),
                name: "Show/ep02.mkv".to_string(),
            },
        ];
        assert_eq!(
            collect_compress_roots(&files),
            vec![PathBuf::from("/media")]
        );
    }

    #[test]
    fn season_key_standard_sxxexx() {
        assert_eq!(
            season_key("Batwheels.S02E32-E33.1080p.NF.WEB-DL.DDP5.1.H.264.DUAL-BiOMA"),
            Some("Batwheels.S02".into())
        );
        assert_eq!(
            season_key("Show.Name.S01E01.720p.BluRay"),
            Some("Show.Name.S01".into())
        );
        assert_eq!(season_key("Series.s03e05.HDTV"), Some("Series.S03".into()));
    }

    #[test]
    fn season_key_no_season_returns_none() {
        assert_eq!(season_key("Random.Movie.2024.1080p"), None);
        assert_eq!(season_key("file"), None);
    }

    #[test]
    fn is_artifact_entry_matches_nfo_and_nzb_case_insensitively() {
        assert!(is_artifact_entry(Path::new("Show.nfo")));
        assert!(is_artifact_entry(Path::new("Show.NZB")));
        assert!(is_artifact_entry(Path::new("/a/b/c.NfO")));
        assert!(!is_artifact_entry(Path::new("Show.mkv")));
        assert!(!is_artifact_entry(Path::new("Show")));
        assert!(!is_artifact_entry(Path::new("nfo")));
    }

    #[test]
    fn top_level_entries_skips_generated_artifacts() {
        let dir = std::env::temp_dir().join(format!(
            "pesto_each_artifact_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ep01.mkv"), b"x").unwrap();
        // Orphan artifacts left in the input directory by a previous run.
        std::fs::write(dir.join("ep01.nfo"), b"x").unwrap();
        std::fs::write(dir.join("ep01.nzb"), b"x").unwrap();

        let names: Vec<String> = top_level_entries(&dir)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["ep01.mkv"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
