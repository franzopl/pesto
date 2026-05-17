//! `pesto` — fast, lean Usenet poster.
//!
//! Parses the CLI, resolves the configuration, posts the given files to Usenet
//! and writes an `.nzb` file describing the result.

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use pesto::compress::{compress, random_password, ArchiveFormat};
use pesto::config::{self, parse_upload_rate, Config, FileConfig, ObfuscateMode, Overrides};
use pesto::nzb::NzbMeta;
use pesto::poster::PostedSegment;

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

    /// Obfuscation mode: `none`, `subject` or `full`. A bare `--obfuscate`
    /// means `full` [config: posting.obfuscate, default none].
    #[arg(long, value_name = "MODE", value_enum, num_args = 0..=1,
          default_missing_value = "full", require_equals = true)]
    obfuscate: Option<ObfuscateMode>,

    /// Percentage of PAR2 recovery data to generate; 0 disables it
    /// [config: posting.par2, default 10].
    #[arg(long, value_name = "PERCENT")]
    par2: Option<u8>,

    /// Only generate PAR2 files next to the sources; do not post.
    #[arg(long)]
    par2_only: bool,

    /// Skip network posting and just measure generation speed.
    #[arg(long)]
    dry_run: bool,

    /// Disable upload resume: ignore any existing state file and start fresh.
    #[arg(long)]
    no_resume: bool,

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

    /// Skip the automatic NZB upload to the configured indexer for this run.
    #[arg(long)]
    no_upload: bool,

    /// `Date:` header for each article: `now` (current time), `random`
    /// (random time within the last 30 days), or a fixed RFC 2822 timestamp.
    /// Omit to let the server supply the date [config: posting.date].
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

    /// Shell command to execute after each successful upload. The command
    /// receives upload details via environment variables:
    /// `PESTO_NZB`, `PESTO_NFO`, `PESTO_NAME`, `PESTO_BYTES`,
    /// `PESTO_GROUP`, `PESTO_PASSWORD`, `PESTO_SERVER`
    /// [config: output.post_hook].
    #[arg(long, value_name = "CMD")]
    post_hook: Option<String>,

    /// Skip writing to the shared upload history
    /// (~/.config/upapasta/history.jsonl) for this run
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
    /// --each. On completion each entry is moved to --watch-done (or deleted).
    /// Exits cleanly on SIGTERM / Ctrl-C after finishing any in-progress upload.
    #[arg(long, value_name = "DIR")]
    watch: Option<PathBuf>,

    /// Destination directory for entries processed by --watch. When omitted,
    /// completed entries are deleted.
    #[arg(long, value_name = "DIR")]
    watch_done: Option<PathBuf>,

    /// How often (in seconds) to poll the watched directory for new entries
    /// [config: watch.poll_interval, default 30].
    #[arg(long, value_name = "SECS", default_value = "30")]
    watch_interval: u64,

    /// Files or directories to post. A directory is walked recursively and
    /// every file inside it is posted, keeping the folder structure.
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
            resume: if self.no_resume { Some(false) } else { None },
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
                if pw.is_empty() { random_password() } else { pw.to_string() }
            }),
            nzb_name: self.nzb_name.clone(),
            nzb_password: self.nzb_password.clone(),
            nzb_category: self.nzb_category.clone(),
            nzb_dir: self.nzb_dir.as_ref().map(|p| p.to_string_lossy().into_owned()),
            no_upload: self.no_upload,
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
            post_hook: self.post_hook.clone(),
            nfo: if self.nfo { Some(true) } else { None },
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
}

/// The result of a single upload (one entry in `--each` / `--season`).
struct UploadResult {
    segments: Vec<PostedSegment>,
    cancelled: bool,
    had_failures: bool,
}

/// Run one complete upload: expand `entry_paths`, compress, post, write NZB.
///
/// Returns the posted segments so the caller can build a consolidated season NZB.
async fn run_single_upload(
    params: &UploadParams,
    entry_paths: &[PathBuf],
    entry_label: &str,
) -> Result<UploadResult> {
    let config = &params.config;
    let upload_start = std::time::Instant::now();

    let mut inputs = pesto::walk::expand_inputs(entry_paths)?;
    let (file_count, folder_count, total_bytes) = upload_summary(&inputs);

    let (progress_tx, renderer) = if params.json_mode {
        pesto::progress::spawn_json_emitter()
    } else {
        pesto::progress::spawn_terminal_renderer()
    };

    // ── Compression ──────────────────────────────────────────────────────────
    let compress_format_str: Option<String> = config
        .compress_format
        .clone()
        .or_else(|| {
            if config.compress_password.is_some() {
                Some("7z".to_string())
            } else {
                None
            }
        });
    let effective_password: Option<String> = config.compress_password.clone();

    let compress_temp_dir: Option<PathBuf>;
    if let Some(fmt_str) = &compress_format_str {
        let format = ArchiveFormat::parse(fmt_str)
            .ok_or_else(|| anyhow::anyhow!(
                "unknown compression format `{fmt_str}`; supported: 7z, zip, rar"
            ))?;

        if format == ArchiveFormat::Rar && effective_password.is_some() {
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

        let archive_stem = if config.obfuscate == ObfuscateMode::Full {
            pesto::article::obfuscated_name()
        } else {
            archive_stem
        };

        let tmp_dir = std::env::temp_dir()
            .join(format!("pesto_compress_{}_{}", std::process::id(), entry_label));
        compress_temp_dir = Some(tmp_dir.clone());

        let fs_paths: Vec<PathBuf> = collect_compress_roots(&inputs);
        let compress_input_bytes: u64 = fs_paths.iter().map(|p| dir_or_file_size(p)).sum();

        let _ = progress_tx.send(pesto::progress::ProgressEvent::CompressStarted {
            total_bytes: compress_input_bytes,
        });

        let archive_path_for_poll = tmp_dir.join(format!("{}.{}", archive_stem, format.extension()));
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

        let archive_name = result.path
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

    // Derive NZB path: --out > nzb_default > nzb_dir/<stem>.nzb > ./<stem>.nzb
    // Always use the original entry_paths for the stem so obfuscation/compression
    // does not leak the randomised archive name into the output filenames.
    let nzb_out_path: Option<PathBuf> = params
        .out
        .clone()
        .or_else(|| params.nzb_default.as_deref().map(PathBuf::from))
        .or_else(|| {
            let stem = entry_paths
                .first()
                .and_then(|p| if p.is_dir() { p.file_name() } else { p.file_stem() })
                .map(|s| s.to_string_lossy().into_owned())
                .or_else(|| upload_root(&inputs))
                .or_else(|| {
                    inputs.first().map(|f| {
                        PathBuf::from(f.name.split('/').next().unwrap_or(&f.name))
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    })
                })?;
            let filename = format!("{stem}.nzb");
            let path = if let Some(dir) = &config.nzb_dir {
                expand_tilde(dir).join(&filename)
            } else {
                PathBuf::from(filename)
            };
            Some(path)
        });
    let resume_path: Option<PathBuf> = nzb_out_path
        .as_ref()
        .map(|p| p.with_extension("pesto-state"));

    let outcome = pesto::poster::post_files_with_progress(
        config,
        &inputs,
        Some(progress_tx),
        resume_path.as_deref(),
    )
    .await?;
    let _ = renderer.await;

    if !params.json_mode {
        if config.par2_only {
            println!("PAR2 generation complete.");
        } else {
            println!("posted {} segment(s)", outcome.segments.len());
        }

        let files_word = if file_count == 1 { "file" } else { "files" };
        let size = pesto::progress::format_size(total_bytes);
        if folder_count > 0 {
            let folders_word = if folder_count == 1 { "subfolder" } else { "subfolders" };
            println!("upload: {file_count} {files_word} · {folder_count} {folders_word} · {size}");
        } else {
            println!("upload: {file_count} {files_word} · {size}");
        }
    }

    if outcome.cancelled {
        eprintln!("interrupted — stopped before posting every requested segment");
    }
    if !outcome.failures.is_empty() {
        eprintln!("{} segment(s) failed:", outcome.failures.len());
        for failure in &outcome.failures {
            eprintln!("  - {failure}");
        }
    }

    // Write NZB.
    let out = nzb_out_path.clone();

    let nzb_xml: Option<String> = if let Some(out) = &out {
        if !config.par2_only {
            if outcome.segments.is_empty() {
                eprintln!("no segments posted — skipping nzb output");
                None
            } else {
                let nzb_meta = NzbMeta {
                    name: config.nzb_name.clone(),
                    password: config
                        .nzb_password
                        .clone()
                        .or_else(|| effective_password.clone()),
                    category: config.nzb_category.clone(),
                };
                let xml = pesto::nzb::generate(
                    &config.from,
                    &config.groups,
                    &outcome.segments,
                    &nzb_meta,
                );
                tokio::fs::write(out, &xml)
                    .await
                    .with_context(|| format!("writing nzb file `{}`", out.display()))?;
                if params.json_mode {
                    let path_esc = out.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
                    println!(r#"{{"type":"nzb_written","path":"{path_esc}"}}"#);
                } else {
                    println!("wrote nzb: {}", out.display());
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
                    pesto::history::record_upload(&pesto::history::UploadRecord {
                        name: entry_label,
                        obfuscated_name: obf_name,
                        password: effective_password.as_deref(),
                        total_bytes,
                        group: config.groups.first().map(String::as_str),
                        server: Some(config.host.as_str()),
                        par2_redundancy: par2_pct,
                        duration_secs: upload_start.elapsed().as_secs_f64(),
                        nzb_path: Some(&out.display().to_string()),
                        subject: config.nzb_name.as_deref().or(Some(entry_label)),
                    });
                }

                Some(xml)
            }
        } else {
            None
        }
    } else {
        None
    };

    // Upload to indexer when configured and not suppressed.
    if let Some(xml) = nzb_xml {
        if !config.no_upload {
            if let Some(url) = &config.indexer_url {
                if let Some(api_key) = &config.indexer_api_key {
                    let nzb_name = out
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "upload.nzb".into());
                    let cat = config
                        .indexer_category
                        .as_deref()
                        .or(config.nzb_category.as_deref());
                    match pesto::indexer::upload_nzb(url, api_key, cat, &nzb_name, xml).await {
                        Ok(()) => println!("uploaded nzb to indexer: {url}"),
                        Err(e) => eprintln!("indexer upload failed: {e}"),
                    }
                }
            }
        }
    }

    // Send completion notifications.
    let notify_enabled = config.notify.unwrap_or(true)
        && (config.notify_webhook.is_some() || config.notify_ntfy.is_some());
    if notify_enabled && !config.par2_only && !config.dry_run {
        let had_failures = outcome.cancelled || !outcome.failures.is_empty();
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

    // Generate .nfo and run post-upload hook when the upload succeeded.
    let upload_ok = !outcome.cancelled && outcome.failures.is_empty();
    if upload_ok && !config.par2_only && !config.dry_run {
        // Derive the .nfo path next to the .nzb (or next to the source).
        let nfo_path: Option<PathBuf> = if config.nfo {
            let base = out
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
                    None => None,
                }
            } else {
                None
            }
        } else {
            None
        };

        let hook_env = HookEnv {
            nzb_path: out.as_deref(),
            nfo_path: nfo_path.as_deref(),
            name: entry_label,
            total_bytes,
            group: config.groups.first().map(String::as_str),
            password: effective_password.as_deref(),
            server: &config.host,
        };

        // Run --post-hook command.
        if let Some(cmd) = &config.post_hook {
            run_post_hook(cmd, &hook_env);
        }

        // Run every executable script found in ~/.config/pesto/hooks/.
        if let Some(hooks_dir) = pesto::config::config_dir().map(|d| d.join("hooks")) {
            run_hooks_dir(&hooks_dir, &hook_env);
        }
    }

    // Cleanup temp dirs.
    if let Some(dir) = compress_temp_dir {
        let _ = std::fs::remove_dir_all(&dir);
    }

    Ok(UploadResult {
        segments: outcome.segments,
        cancelled: outcome.cancelled,
        had_failures: !outcome.failures.is_empty(),
    })
}

/// Enumerate top-level entries of `dir` (files and subdirectories), sorted by name.
fn top_level_entries(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory `{}`", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
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
) -> Result<(Vec<PostedSegment>, bool, bool)> {
    // Collect all entries from every directory argument.
    let mut entries: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let md = std::fs::metadata(dir)
            .with_context(|| format!("reading `{}`", dir.display()))?;
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
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        jobs
    };

    let semaphore = Arc::new(tokio::sync::Semaphore::new(effective_jobs));
    let mut all_segments: Vec<PostedSegment> = Vec::new();
    let mut any_cancelled = false;
    let mut any_failures = false;

    let mut handles = Vec::new();
    for entry in &entries {
        let entry = entry.clone();
        let params = Arc::clone(&params);
        let sem = Arc::clone(&semaphore);
        let label = entry
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "entry".to_string());

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            if !params.json_mode {
                println!("\n── {} ──", label);
            }
            run_single_upload(&params, &[entry], &label).await
        });
        handles.push(handle);
    }

    for handle in handles {
        match handle.await {
            Ok(Ok(result)) => {
                all_segments.extend(result.segments);
                if result.cancelled { any_cancelled = true; }
                if result.had_failures { any_failures = true; }
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

    // Write consolidated season NZB when requested.
    if let Some(season_path) = season_nzb {
        if !all_segments.is_empty() {
            let config = &params.config;
            let nzb_meta = NzbMeta {
                name: config.nzb_name.clone(),
                password: config.nzb_password.clone(),
                category: config.nzb_category.clone(),
            };
            let xml = pesto::nzb::generate(
                &config.from,
                &config.groups,
                &all_segments,
                &nzb_meta,
            );
            tokio::fs::write(&season_path, &xml)
                .await
                .with_context(|| format!("writing season nzb `{}`", season_path.display()))?;
            if !params.json_mode {
                println!("\nwrote season nzb: {}", season_path.display());
            } else {
                let path_esc = season_path.display().to_string()
                    .replace('\\', "\\\\").replace('"', "\\\"");
                println!(r#"{{"type":"nzb_written","path":"{path_esc}","season":true}}"#);
            }

            // Upload consolidated NZB to indexer when configured.
            if !config.no_upload {
                if let Some(url) = &config.indexer_url {
                    if let Some(api_key) = &config.indexer_api_key {
                        let nzb_name = season_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "season.nzb".into());
                        let cat = config
                            .indexer_category
                            .as_deref()
                            .or(config.nzb_category.as_deref());
                        match pesto::indexer::upload_nzb(url, api_key, cat, &nzb_name, xml).await {
                            Ok(()) => println!("uploaded season nzb to indexer: {url}"),
                            Err(e) => eprintln!("indexer upload failed (season nzb): {e}"),
                        }
                    }
                }
            }
        }
    }

    Ok((all_segments, any_cancelled, any_failures))
}

/// Run `--watch DIR`: poll for new entries and post each one automatically.
///
/// Exits cleanly on SIGTERM or Ctrl-C after finishing any in-progress upload.
async fn run_watch(
    params: Arc<UploadParams>,
    watch_dir: &Path,
    watch_done: Option<&Path>,
    poll_interval: u64,
    jobs: usize,
) -> Result<()> {
    use tokio::signal;

    eprintln!("watching {} (poll every {}s)", watch_dir.display(), poll_interval);

    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_clone = Arc::clone(&shutdown);

    // Listen for Ctrl-C / SIGTERM on a background task.
    tokio::spawn(async move {
        let ctrl_c = async { signal::ctrl_c().await.ok(); };
        #[cfg(unix)]
        let sigterm = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler")
                .recv()
                .await;
        };
        #[cfg(not(unix))]
        let sigterm = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm => {},
        }
        eprintln!("\nshutdown requested — finishing in-progress uploads");
        shutdown_clone.notify_waiters();
    });

    let mut seen: HashSet<PathBuf> = HashSet::new();
    // Pre-populate seen with whatever is already there so we don't re-post on startup.
    if let Ok(existing) = top_level_entries(watch_dir) {
        for e in existing {
            seen.insert(e);
        }
    }

    let semaphore = Arc::new(tokio::sync::Semaphore::new(if jobs == 0 {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    } else {
        jobs
    }));

    loop {
        // Check for shutdown between polls.
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(poll_interval)) => {}
            _ = shutdown.notified() => { break; }
        }

        let entries = match top_level_entries(watch_dir) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("watch: error reading {}: {e}", watch_dir.display());
                continue;
            }
        };

        for entry in entries {
            if seen.contains(&entry) {
                continue;
            }
            seen.insert(entry.clone());

            let params = Arc::clone(&params);
            let sem = Arc::clone(&semaphore);
            let watch_done = watch_done.map(PathBuf::from);
            let label = entry
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "entry".to_string());

            tokio::spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore closed");
                if !params.json_mode {
                    println!("\n── watch: {} ──", label);
                }
                match run_single_upload(&params, std::slice::from_ref(&entry), &label).await {
                    Ok(_) => {
                        // Move or delete the completed entry.
                        if let Some(done_dir) = &watch_done {
                            let dest = done_dir.join(entry.file_name().unwrap_or_default());
                            if let Err(e) = std::fs::rename(&entry, &dest) {
                                eprintln!("watch: could not move `{}` to `{}`: {e}",
                                    entry.display(), dest.display());
                            }
                        } else {
                            // Delete completed entry.
                            let is_dir = entry.is_dir();
                            let result = if is_dir {
                                std::fs::remove_dir_all(&entry)
                            } else {
                                std::fs::remove_file(&entry)
                            };
                            if let Err(e) = result {
                                eprintln!("watch: could not delete `{}`: {e}", entry.display());
                            }
                        }
                    }
                    Err(e) => eprintln!("watch: upload failed for `{}`: {e:#}", entry.display()),
                }
            });
        }
    }

    // Wait for all in-progress uploads (drain the semaphore).
    let effective_jobs = if jobs == 0 {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    } else {
        jobs
    };
    let _ = semaphore.acquire_many(effective_jobs as u32).await;
    eprintln!("watch: all uploads finished, exiting");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // `pesto --config` with no value: launch the interactive setup wizard.
    if matches!(cli.config, Some(None)) {
        return run_wizard();
    }

    // `pesto` with nothing to post and no --watch: show the orientation screen.
    let has_work = !cli.files.is_empty() || cli.watch.is_some();
    if !has_work {
        print_welcome();
        return Ok(());
    }

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
    let config = Arc::new(Config::resolve(file_config, cli.overrides())?);
    let json_mode = cli.output_format.trim().eq_ignore_ascii_case("json");

    let params = Arc::new(UploadParams {
        config: Arc::clone(&config),
        archive_password_raw: cli.archive_password.clone(),
        nzb_default: nzb_default.map(|s| s.to_string()),
        json_mode,
        out: cli.out.clone(),
        write_history: config.history,
    });

    // ── --watch mode ──────────────────────────────────────────────────────────
    if let Some(watch_dir) = &cli.watch {
        return run_watch(
            params,
            watch_dir,
            cli.watch_done.as_deref(),
            cli.watch_interval,
            cli.jobs,
        )
        .await;
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
                        Some(PathBuf::from(format!("{name}.nzb")))
                    } else {
                        None
                    }
                })
            })
        } else {
            None
        };

        let (_, any_cancelled, any_failures) =
            run_batch(params, &cli.files, cli.jobs, season_nzb).await?;

        if any_cancelled {
            std::process::exit(130);
        }
        if any_failures {
            std::process::exit(1);
        }
        return Ok(());
    }

    // ── Single upload (normal mode) ───────────────────────────────────────────
    let label = cli
        .files
        .first()
        .and_then(|p| if p.is_dir() { p.file_name() } else { p.file_stem() })
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("{}", std::process::id()));
    let result = run_single_upload(&params, &cli.files, &label).await?;

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
    group: Option<&'a str>,
    password: Option<&'a str>,
    server: &'a str,
}

fn apply_hook_env(child: &mut std::process::Command, env: &HookEnv<'_>) {
    child.env("PESTO_NAME", env.name);
    child.env("PESTO_BYTES", env.total_bytes.to_string());
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

/// Run the interactive setup wizard.
fn run_wizard() -> Result<()> {
    let path = config::default_config_path()
        .context("cannot locate a config directory: set $HOME or $XDG_CONFIG_HOME")?;

    println!("pesto setup — answer a few questions to create your config.");
    println!("Press Enter to accept the [default] shown in brackets.\n");

    if path.exists() {
        let overwrite = ask("Config already exists; overwrite it? (y/N)", "n")?;
        if !overwrite.eq_ignore_ascii_case("y") {
            println!("Aborted; existing config kept.");
            return Ok(());
        }
    }

    let host = ask_required("NNTP server hostname")?;
    let port = ask("Server port", "563")?;
    let ssl = ask("Use TLS? (Y/n)", "y")?;
    let ssl = !ssl.eq_ignore_ascii_case("n");
    let connections = ask("Parallel connections", "10")?;
    let username = ask("Username (blank for none)", "")?;
    let password = if username.is_empty() {
        String::new()
    } else {
        ask("Password", "")?
    };
    let groups = ask("Newsgroups (comma-separated)", "alt.binaries.test")?;
    let from = ask("From header (blank = random identity per run)", "")?;
    let par2 = ask("PAR2 recovery percentage (0 disables)", "10")?;

    let mut toml = String::new();
    toml.push_str("# pesto configuration — generated by `pesto --config`.\n\n");
    toml.push_str("[server]\n");
    toml.push_str(&format!("host = \"{}\"\n", esc(&host)));
    toml.push_str(&format!("port = {}\n", port.trim()));
    toml.push_str(&format!("ssl = {ssl}\n"));
    toml.push_str(&format!("connections = {}\n", connections.trim()));
    toml.push('\n');

    if !username.is_empty() {
        toml.push_str("[auth]\n");
        toml.push_str(&format!("username = \"{}\"\n", esc(&username)));
        toml.push_str(&format!("password = \"{}\"\n\n", esc(&password)));
    }

    toml.push_str("[posting]\n");
    let group_list = groups
        .split(',')
        .map(|g| format!("\"{}\"", esc(g.trim())))
        .collect::<Vec<_>>()
        .join(", ");
    toml.push_str(&format!("groups = [{group_list}]\n"));
    if from.is_empty() {
        toml.push_str("# from omitted: each run posts under a random identity.\n");
    } else {
        toml.push_str(&format!("from = \"{}\"\n", esc(&from)));
    }
    toml.push_str(&format!("par2 = {}\n", par2.trim()));

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory `{}`", parent.display()))?;
    }
    std::fs::write(&path, toml)
        .with_context(|| format!("writing config file `{}`", path.display()))?;

    println!("\nWrote {}", path.display());
    println!("pesto will load it automatically. Post a file with: pesto <FILE>");
    Ok(())
}

fn ask(label: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default}]: ");
    }
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("reading from stdin")?;
    let line = line.trim().to_string();
    Ok(if line.is_empty() {
        default.to_string()
    } else {
        line
    })
}

fn ask_required(label: &str) -> Result<String> {
    loop {
        let value = ask(label, "")?;
        if !value.is_empty() {
            return Ok(value);
        }
        println!("  (required — please enter a value)");
    }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
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
}
