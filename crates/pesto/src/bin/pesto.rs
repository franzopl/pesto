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
use tracing::{error, info};

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
given, pesto loads it from the OS-standard location: $XDG_CONFIG_HOME/pesto/config.toml
(or, failing that, ~/.config/pesto/config.toml) on Linux/macOS, or
%APPDATA%\\pesto\\config.toml on Windows — NOT ~/.config, which the Windows
build never checks. Create that file interactively with `pesto --config`,
which prints the exact path it wrote to.

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
  pesto --each --ext mkv ./shows/ post only .mkv per episode, skip .srt/etc.

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

    /// Download the latest pesto-v* release from GitHub for this platform,
    /// verify its checksum, and replace the running binary with it. Exits
    /// without touching anything else (no upload, no config load).
    #[arg(long)]
    update: bool,

    /// NNTP server hostname [config: server.host].
    #[arg(short = 's', long, value_name = "HOST")]
    host: Option<String>,

    /// NNTP server port [config: server.port, default 563].
    #[arg(short = 'P', long, value_name = "PORT")]
    port: Option<u16>,

    /// Disable TLS; connect in plaintext [config: server.ssl].
    #[arg(long)]
    no_ssl: bool,

    /// Number of parallel connections [config: server.connections, default 4].
    #[arg(short = 'n', long, value_name = "N")]
    connections: Option<usize>,

    /// Authentication username [config: auth.username].
    #[arg(short = 'u', long, value_name = "USER")]
    username: Option<String>,

    /// Authentication password for the NNTP server [config: auth.password].
    #[arg(short = 'p', long = "auth-password", value_name = "PASS")]
    password: Option<String>,

    /// `From` header for posted articles; omitted means a random identity
    /// [config: posting.from].
    #[arg(short = 'f', long, value_name = "ADDRESS")]
    from: Option<String>,

    /// Newsgroups to post to (repeat or comma-separate) [config: posting.groups].
    /// With more than one, one is chosen at random per run to spread posts
    /// across the pool; join names with '+' in a single value to cross-post
    /// to all of them at once instead, e.g. `-g alt.a+alt.b`.
    #[arg(short = 'g', long, value_name = "GROUP", value_delimiter = ',')]
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

    /// Obfuscation mode: `none`, `full`, `full-shared`. A bare `--obfuscate`
    /// means `full`. `full-shared` is like `full` but reuses one random name
    /// across every file in the release (archive + PAR2 volumes) so indexers
    /// can still group them [config: posting.obfuscate, default none].
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

    /// Directory where intermediate PAR2 files are written during posting,
    /// before they're read back and posted. Defaults to the OS temp
    /// directory (e.g. /tmp), which may sit on a different filesystem —
    /// with less free space or a stricter disk quota — than the destination
    /// disk. Ignored with --par2-only, which writes PAR2 files next to the
    /// sources instead [config: posting.par2_temp_dir].
    #[arg(long, value_name = "DIR")]
    par2_temp_dir: Option<String>,

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

    /// Generate all PAR2 recovery data before posting anything, instead of
    /// computing it concurrently with the data upload (the default). Every
    /// data file, the PAR2 index and every volume are then posted back to
    /// back with no gap between them. This trades a longer wait before the
    /// first article goes out for a release whose articles all land within
    /// a tight time window — mirrors the two-phase workflow of tools like
    /// ParPar+nyuu (generate, then post), instead of pesto's usual
    /// streaming/overlapped pipeline where PAR2 encoding runs concurrently
    /// with the upload
    /// [config: posting.par2_before_upload, default false].
    #[arg(long)]
    par2_before_upload: bool,

    /// Skip network posting and just measure generation speed.
    #[arg(long)]
    dry_run: bool,

    /// Resume an interrupted upload from where it left off. Without this
    /// flag pesto always starts fresh, even if a state file exists from a
    /// previous incomplete run at the same output path — progress is always
    /// saved on an incomplete run regardless of this flag, but only loaded
    /// back (to skip already-posted segments) when --resume is passed. With
    /// --compress, the archive is always rebuilt from scratch and its
    /// segments can't be skipped — only its identity and any sent-but-
    /// unconfirmed article carry over. See the README's "Upload resume"
    /// section for the full picture
    /// [config: output.resume = true].
    #[arg(long)]
    resume: bool,

    /// Maximum upload rate across all connections (e.g. "50 MiB/s", "10 MB/s").
    /// 0 or omitted means unlimited [config: posting.upload_rate].
    #[arg(long, value_name = "RATE")]
    rate: Option<String>,

    /// Bundle all files into an archive before posting. Optional FORMAT:
    /// `7z` (default, store mode), `zip` (via 7z), or `rar` (requires rar in
    /// PATH) [config: compression.format].
    #[arg(long, value_name = "FORMAT", num_args = 0..=1, default_missing_value = "7z")]
    compress: Option<String>,

    /// Directory where the archive built by --compress is staged, before
    /// it's read back and posted. Defaults to the OS temp directory (e.g.
    /// /tmp), which may sit on a different filesystem — with less free
    /// space or a stricter disk quota — than the destination disk
    /// [config: compression.temp_dir].
    #[arg(long, value_name = "DIR")]
    compress_temp_dir: Option<String>,

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

    /// TMDb reference written to `<meta type="tmdbid">` in the `.nzb`, as
    /// `movie/<id>` or `tv/<id>` (`movie:<id>` / `tv:<id>` also accepted).
    /// When `--nzb-category` is not set, the category defaults to `movies`
    /// or `tv` accordingly. Also added as a line in the `.nfo` when `--nfo`
    /// is set. Aliased as `--tmdb-id`.
    #[arg(long, alias = "tmdb-id", value_name = "TYPE/ID")]
    tmdb: Option<String>,

    /// IMDb ID written to `<meta type="imdbid">` in the `.nzb`, e.g.
    /// `tt1234567`. The `tt` prefix is optional and added automatically
    /// (`133093` normalizes to `tt0133093`). Also added as a line in the
    /// `.nfo` when `--nfo` is set. Aliased as `--imdb`.
    #[arg(long, alias = "imdb", value_name = "ID")]
    imdb_id: Option<String>,

    /// TheTVDB ID written to `<meta type="tvdbid">` in the `.nzb`, e.g.
    /// `81189`. Also added as a line in the `.nfo` when `--nfo` is set.
    /// Aliased as `--tvdb`.
    #[arg(long, alias = "tvdb", value_name = "ID")]
    tvdb_id: Option<String>,

    /// MyAnimeList ID written to `<meta type="malid">` in the `.nzb`, e.g.
    /// `1535`. Also added as a line in the `.nfo` when `--nfo` is set.
    /// Aliased as `--mal`.
    #[arg(long, alias = "mal", value_name = "ID")]
    mal_id: Option<String>,

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

    /// Prefix every subject with a `[filenum/total]` release-wide file
    /// counter, e.g. `[3/15] "movie.mkv" yEnc (1/1875)`, counting every file
    /// in the release (data files plus the PAR2 index and volumes). Some
    /// posting tools (e.g. nyuu) emit this by default and some indexers may
    /// key their grouping heuristics off it. On by default for `--obfuscate
    /// none` and `full-shared`, which already accept cross-file correlation
    /// by wire metadata as part of their own design (bare filename, or a
    /// shared prefix/From); off by default for `full`/`paranoid`, whose
    /// whole point is preventing exactly that. Pass --no-file-counter to
    /// force it off regardless of mode. See `ROADMAP.md` "Subject file
    /// counter" [config: posting.file_counter].
    #[arg(long)]
    file_counter: bool,

    /// Force the `[filenum/total]` subject counter off, overriding the
    /// per-`--obfuscate`-mode default [config: posting.file_counter].
    #[arg(long)]
    no_file_counter: bool,

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
    /// `PESTO_GROUP`, `PESTO_GROUPS`, `PESTO_SERVER`, `PESTO_SERVERS`,
    /// `PESTO_CATEGORY`, `PESTO_NZB_NAME`, `PESTO_OBFUSCATE`, `PESTO_PAR2`,
    /// `PESTO_TAGS`
    /// Can be specified multiple times. [config: output.pre_hooks].
    #[arg(long, value_name = "CMD", action = clap::ArgAction::Append)]
    pre_hook: Vec<String>,

    /// Shell command to execute after each successful upload. The command
    /// receives upload details via environment variables:
    /// `PESTO_NZB`, `PESTO_NFO`, `PESTO_NAME`, `PESTO_BYTES`,
    /// `PESTO_INPUT_PATHS`, `PESTO_GROUP`, `PESTO_GROUPS`, `PESTO_PASSWORD`,
    /// `PESTO_SERVER`, `PESTO_SERVERS`, `PESTO_CATEGORY`, `PESTO_NZB_NAME`,
    /// `PESTO_OBFUSCATE`, `PESTO_PAR2`, `PESTO_TAGS`
    /// Can be specified multiple times. [config: output.post_hooks].
    #[arg(long, value_name = "CMD", action = clap::ArgAction::Append)]
    post_hook: Vec<String>,

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
    /// Combine with --jobs for parallel uploads. Also applies to directories
    /// detected by --watch: each one is split per top-level entry instead of
    /// posted as a single combined NZB.
    #[arg(long)]
    each: bool,

    /// Like --each, but also produces one consolidated NZB for the whole
    /// directory. The consolidated NZB is named after the directory. Also
    /// applies to directories detected by --watch.
    #[arg(long)]
    season: bool,

    /// Number of independent uploads to run in parallel when --each or
    /// --season is active. Default 1 (sequential). 0 means one per logical CPU.
    #[arg(long, value_name = "N", default_value = "1")]
    jobs: usize,

    /// Restrict uploads to files with one of these extensions
    /// (comma-separated, case-insensitive, without the dot: `--ext mkv,mp4`).
    /// A directory argument is still walked as usual; only files that don't
    /// match are dropped. Most useful with --each/--season/--watch to skip
    /// subtitle, sample, or other extra files bundled next to the video.
    /// Default: no filtering (every file is included).
    #[arg(long, value_name = "EXT", value_delimiter = ',')]
    ext: Vec<String>,

    /// Watch DIR for new entries and post each one automatically. A directory
    /// entry is posted as a single combined NZB by default, or split per
    /// top-level entry (one NZB per file) when --each or --season is also
    /// passed. On completion each entry is moved to --watch-done (if set);
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

    /// Confirm every posted article via a streaming STAT check that runs
    /// concurrently with the upload — each article is checked --check-delay
    /// seconds after it posts, and misses are reposted automatically. On by
    /// default; pass --no-check to disable
    /// [config: posting.check, default true].
    #[arg(long)]
    check: bool,

    /// Disable the streaming STAT check [config: posting.check].
    #[arg(long)]
    no_check: bool,

    /// Seconds to wait after an article posts before its first STAT check
    /// [config: posting.check_delay, default 5].
    #[arg(long, value_name = "SECS")]
    check_delay: Option<u64>,

    /// Number of STAT attempts per posted copy before triggering a repost;
    /// 20 seconds between each retry [config: posting.check_retries, default 3].
    #[arg(long, value_name = "N")]
    check_retries: Option<u32>,

    /// Number of dedicated parallel NNTP connections for the streaming check
    /// queue, carved out of --connections (not opened on top of it, so the
    /// total never exceeds what you configured); defaults to a small pool
    /// [config: posting.check_connections].
    #[arg(long, value_name = "N")]
    check_connections: Option<usize>,

    /// Number of times to re-post an article the STAT pass still can't find,
    /// each followed by another full STAT pass over the remaining missing
    /// articles. A single round (the default) covers a transient drop; raise
    /// this on providers with slower or less reliable propagation
    /// [config: posting.check_post_retries, default 1].
    #[arg(long, value_name = "N")]
    check_post_retries: Option<u32>,

    /// Publish the NZB (and run post-upload hooks) even if some articles are
    /// still confirmed missing after every --check-post-retries round.
    /// Without this, pesto refuses to write an NZB it never confirmed is
    /// fully retrievable [config: posting.allow_incomplete_nzb, default false].
    #[arg(long)]
    allow_incomplete_nzb: bool,

    /// After every --check-post-retries round is exhausted, skip the
    /// automatic final recovery pass (see --check-recover-max) if the
    /// still-missing articles exceed this percentage of the release's total
    /// segments — past this point it looks like a systemic problem, not a
    /// handful of unlucky articles, and retrying automatically would just
    /// hammer an already-struggling server
    /// [config: posting.check_recover_percent, default 15].
    #[arg(long, value_name = "PERCENT")]
    check_recover_percent: Option<u8>,

    /// After every --check-post-retries round is exhausted, if the number of
    /// still-missing articles is at or below this count (and within
    /// --check-recover-percent of the release), automatically make one more
    /// repost-and-verify attempt for just those articles before giving up —
    /// cheap enough to be worth doing without a separate --resume run. Set
    /// to 0 to disable
    /// [config: posting.check_recover_max, default 50].
    #[arg(long, value_name = "N")]
    check_recover_max: Option<usize>,

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
            par2_before_upload: if self.par2_before_upload {
                Some(true)
            } else {
                None
            },
            par2_memory_limit: self
                .memory_limit
                .as_ref()
                .and_then(|s| parse_upload_rate(s).ok()),
            par2_temp_dir: self.par2_temp_dir.clone(),
            par2_slice_size: self
                .slice_size
                .as_ref()
                .and_then(|s| parse_upload_rate(s).ok()),
            par2_slice_count: self.slice_count,
            par2_recovery_count: self.recovery_count,
            threads: self.threads,
            simd: Some(self.simd),
            resume: if self.resume { Some(true) } else { None },
            upload_rate: self
                .rate
                .as_deref()
                .map(parse_upload_rate)
                .transpose()
                .unwrap_or(None),
            compress_format: self.compress.clone(),
            compress_temp_dir: self.compress_temp_dir.clone(),
            // None → no password (flag absent, or bare `--password` for
            // auto-random). Some(s) → an explicit password, reused verbatim
            // by every entry under --each/--season/--watch. The bare-flag
            // case is deliberately *not* resolved here: doing so used to
            // bake one random password into `Config` for the whole process,
            // so every entry under --each/--watch silently shared it
            // instead of getting its own (issue #67). It's resolved lazily
            // per upload instead — see `run_single_upload`'s
            // `effective_password` and `run_batch`'s `season_password`.
            compress_password: self
                .archive_password
                .as_deref()
                .and_then(|pw| (!pw.is_empty()).then(|| pw.to_string())),
            nzb_name: self.nzb_name.clone(),
            nzb_password: self.nzb_password.clone(),
            nzb_category: self.nzb_category.clone(),
            nzb_tags: self.nzb_tag.clone(),
            tmdb: self.tmdb.clone(),
            imdb_id: self.imdb_id.clone(),
            tvdb_id: self.tvdb_id.clone(),
            mal_id: self.mal_id.clone(),
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
            file_counter: if self.no_file_counter {
                Some(false)
            } else if self.file_counter {
                Some(true)
            } else {
                None
            },
            message_id_domain: self.message_id_domain.clone(),
            pre_hooks: self.pre_hook.clone(),
            post_hooks: self.post_hook.clone(),
            no_hooks: if self.no_hooks { Some(true) } else { None },
            nfo: if self.nfo { Some(true) } else { None },
            nzb_conflict: if self.no_overwrite {
                Some(pesto::config::NzbConflict::Rename)
            } else {
                self.nzb_conflict
            },
            check: if self.no_check {
                Some(false)
            } else if self.check {
                Some(true)
            } else {
                None
            },
            check_delay_secs: self.check_delay,
            check_retries: self.check_retries,
            check_connections: self.check_connections,
            check_post_retries: self.check_post_retries,
            allow_incomplete_nzb: if self.allow_incomplete_nzb {
                Some(true)
            } else {
                None
            },
            check_recover_percent: self.check_recover_percent,
            check_recover_max: self.check_recover_max,
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
    /// Extensions from `--ext`, lowercased with any leading dot stripped.
    /// Empty means no filtering.
    ext_filter: Vec<String>,
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
    /// Includes the streaming check/repost queue draining, which now runs
    /// concurrently with posting rather than as a separate serial phase.
    post_ms: Option<u128>,
}

/// Resolve the archive password for one upload.
///
/// Priority: `forced` (a season's shared password, passed down from
/// `run_batch`) beats `explicit` (`Config::compress_password` — an
/// explicit `--password VALUE`, meant to be reused verbatim by every entry
/// in the run) beats a freshly-generated random password when `raw` shows a
/// bare `--password` was given (`Some("")`) beats no password at all.
///
/// The bare-flag case is resolved here, per call, rather than once when the
/// CLI is parsed — resolving it once used to bake a single random password
/// into `Config` for the whole process, so every entry under
/// `--each`/`--watch` silently shared it instead of getting its own
/// (issue #67). `run_batch` passes `forced` for a `--season` batch (every
/// episode needs the same password so the merged season NZB only needs
/// one) and `None` otherwise, so a plain `--each` still gets a fresh
/// password per entry through the `raw` fallback below.
fn resolve_entry_password(
    forced: Option<&str>,
    explicit: Option<&str>,
    raw: Option<&str>,
) -> Option<String> {
    forced
        .or(explicit)
        .map(str::to_string)
        .or_else(|| (raw == Some("")).then(random_password))
}

/// Run one complete upload: expand `entry_paths`, compress, post, write NZB.
///
/// Returns the posted segments so the caller can build a consolidated season NZB.
async fn run_single_upload(
    params: &UploadParams,
    entry_paths: &[PathBuf],
    entry_label: &str,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    forced_password: Option<&str>,
) -> Result<UploadResult> {
    let config = &params.config;
    // Resolved once, used for the pre-upload summary, the archive itself,
    // and the .nzb/history/hook metadata alike — so all of them agree on
    // the exact password that ends up protecting this entry's archive.
    let effective_password: Option<String> = resolve_entry_password(
        forced_password,
        config.compress_password.as_deref(),
        params.archive_password_raw.as_deref(),
    );
    let upload_start = std::time::Instant::now();
    let mut timings = PhaseTimings::default();

    let mut inputs = pesto::walk::expand_inputs(entry_paths)?;
    apply_ext_filter(&mut inputs, &params.ext_filter, entry_label)?;
    let (_file_count, _folder_count, total_bytes) = upload_summary(&inputs);
    // Snapshot the pre-compression file list: `inputs` gets overwritten below
    // with the single archive file when --compress is active, but hooks still
    // need the original filenames (e.g. to detect a video file by extension
    // for thumbnail generation) regardless of what was actually posted.
    let original_inputs = inputs.clone();

    // Run pre-hook(s) before anything else (before compression, PAR2, or NNTP).
    // Non-zero exit from any hook aborts the upload immediately.
    // --no-hooks suppresses only the pre-hooks/ directory; --pre-hook always runs
    // (matching the post-hook behaviour established in the PR that fixed no_hooks).
    if !config.dry_run {
        let input_paths_str = inputs
            .iter()
            .map(|f| f.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let pre_obfuscate = match config.obfuscate {
            ObfuscateMode::None => "none",
            ObfuscateMode::Full => "full",
            ObfuscateMode::FullShared => "full-shared",
            ObfuscateMode::Paranoid => "paranoid",
        };
        let pre_groups_str = config.groups.join(":");
        let pre_tags_str = config.nzb_tags.join(" ");
        // No upload has happened yet, so report every server that will get a
        // connection quota (config.host plus extra_servers) rather than just
        // the primary — with [[servers]] all of them start receiving
        // connections immediately, unlike `groups`, where only one is
        // eventually chosen at random.
        let pre_servers_str = config
            .all_servers()
            .map(|s| s.host)
            .collect::<Vec<_>>()
            .join(":");
        let pre_env = HookEnv {
            nzb_path: None,
            nfo_path: None,
            name: entry_label,
            total_bytes,
            input_paths: &input_paths_str,
            group: config.groups.first().map(String::as_str),
            groups: &pre_groups_str,
            password: None,
            server: pre_servers_str.split(':').next().unwrap_or(&config.host),
            servers: &pre_servers_str,
            category: config.nzb_category.as_deref(),
            nzb_name: config.nzb_name.as_deref(),
            obfuscate: pre_obfuscate,
            par2: config.par2,
            tags: &pre_tags_str,
            tmdb_id: config.tmdb_id.as_deref(),
            imdb_id: config.imdb_id.as_deref(),
            tvdb_id: config.tvdb_id.as_deref(),
            mal_id: config.mal_id.as_deref(),
        };

        // Explicit --pre-hook always runs (not suppressed by --no-hooks).
        for cmd in &config.pre_hooks {
            run_pre_hook(cmd, &pre_env)?;
        }

        // Directory scripts are suppressed by --no-hooks.
        if !config.no_hooks {
            if let Some(pre_hooks_dir) = pesto::config::config_dir().map(|d| d.join("pre-hooks")) {
                run_pre_hooks_dir(&pre_hooks_dir, &pre_env)?;
            }
        }
    }

    if !params.json_mode && !params.renderer_opts.quiet && std::io::stderr().is_terminal() {
        pesto::progress::print_tree(&inputs);
        let compress_fmt = config.compress_format.as_deref().or_else(|| {
            if effective_password.is_some() {
                Some("7z")
            } else {
                None
            }
        });
        pesto::progress::print_upload_flags(&pesto::progress::UploadFlags {
            obfuscate: match config.obfuscate {
                ObfuscateMode::None => "none",
                ObfuscateMode::Full => "full",
                ObfuscateMode::FullShared => "full-shared",
                ObfuscateMode::Paranoid => "paranoid",
            },
            compress: compress_fmt,
            password: effective_password.as_deref(),
            par2: config.par2,
            resume: config.resume,
            check: config.check,
        });
    }

    let (progress_tx, renderer) = if params.json_mode {
        pesto::progress::spawn_json_emitter()
    } else {
        pesto::ui::terminal::spawn_renderer_with(params.renderer_opts.clone())
    };

    // Derive NZB stem from: --out > nzb_default > nzb_dir/<stem>.nzb > ./<stem>.nzb
    // Computed from the original entry_paths, before compression, so it never
    // depends on the (possibly obfuscated/randomised) archive name compression
    // produces below.
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

    // ── Compression ──────────────────────────────────────────────────────────
    let compress_format_str: Option<String> = config.compress_format.clone().or_else(|| {
        if effective_password.is_some() {
            Some("7z".to_string())
        } else {
            None
        }
    });

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

        // The obfuscated archive name is normally regenerated fresh on every
        // run — but that means a --resume run can never match this file's
        // segments back up, since the resume key is this very name. When a
        // compatible prior state exists (same posting parameters — see
        // `resume::RunFingerprint`) and already recorded one, reuse it
        // instead of generating a new one; otherwise generate fresh and
        // record it (tracked unconditionally, same as segment state — see
        // issue #18's follow-up discussion) so a *future* --resume can reuse
        // it. `poster::post_files_with_progress_and_cancel` still validates
        // the fingerprint itself, so a genuinely incompatible resume run
        // simply gets a fresh stem here and a wiped segment state there.
        let archive_stem = if config.obfuscate != ObfuscateMode::None {
            reuse_or_generate_archive_stem(resume_path.as_deref(), config)
        } else {
            archive_stem
        };

        let tmp_base = config
            .compress_temp_dir
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        let tmp_dir = tmp_base.join(format!(
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

    let t_post = std::time::Instant::now();
    let outcome = pesto::poster::post_files_with_progress_and_cancel(
        config,
        &inputs,
        Some(progress_tx),
        resume_path.as_deref(),
        cancel.cloned(),
        Some(entry_label),
    )
    .await?;
    let _ = renderer.await;
    timings.post_ms = Some(t_post.elapsed().as_millis());

    // `post_files_with_progress_and_cancel` already retried in-run POST
    // failures (repost_failed_tasks) and ran the streaming STAT check +
    // repost internally, concurrently with the upload. `outcome.still_missing`
    // is the final list of articles that never got confirmed after every
    // repost attempt.
    let cancelled = outcome.cancelled || cancel.is_some_and(|f| f.load(Ordering::Relaxed));
    let check_missing: Vec<String> = if cancelled {
        Vec::new()
    } else {
        outcome.still_missing.clone()
    };

    if !params.json_mode && config.par2_only {
        if cancelled {
            println!("PAR2 generation interrupted.");
        } else {
            println!("PAR2 generation complete.");
        }
    }

    if cancelled {
        // `outcome.cancelled` is set both by a real user cancellation and by a
        // producer error (bad PAR2 geometry, a memory-budget check, file I/O,
        // …) — see `PostOutcome::failure_reason`. Printing the same generic
        // "interrupted" text for both left a run that actually failed with no
        // indication of why, and the same file would then fail identically on
        // every retry with no clue that retrying wouldn't help (issue #57).
        if let Some(reason) = &outcome.failure_reason {
            eprintln!("upload failed: {reason}");
        } else if config.par2_only {
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
    // `check_missing` is already final: `post_files_with_progress_and_cancel`
    // ran the streaming STAT check and every repost attempt internally,
    // concurrently with the upload, so there is no separate repost round to
    // drive here anymore.
    if !cancelled
        && config.check
        && !config.dry_run
        && !config.par2_only
        && !outcome.segments.is_empty()
    {
        if check_missing.is_empty() {
            // Success is already reported: the renderer's final summary shows
            // "all verified" (TTY) and `draw_plain`'s last line carries the
            // check tally (non-TTY/-v). A second "check: all N verified" line
            // here would just duplicate it.
        } else {
            eprintln!(
                "check: {} article(s) still missing after every repost attempt:",
                check_missing.len()
            );
            for id in &check_missing {
                eprintln!("  - {id}");
            }
            error!(
                count = check_missing.len(),
                ids = ?check_missing,
                "check: articles still missing after every repost attempt"
            );
        }
    }

    // If segments still failed after retry, refuse to write the NZB — it
    // would be incomplete. The resume state already has all successfully
    // posted segments so the user can continue with --resume.
    let has_post_failures =
        !outcome.failed_tasks.is_empty() && !config.dry_run && !config.par2_only;
    // Set when a STAT pass still can't find some articles after every
    // --check-post-retries round — a *different* kind of incompleteness
    // than a POST that never got acknowledged at all. `--allow-incomplete-nzb`
    // opts back into publishing only this kind of gap (e.g. relying on PAR2
    // recovery); a genuine POST failure always blocks regardless of the flag.
    let has_confirmed_missing = !check_missing.is_empty() && !config.dry_run && !config.par2_only;
    let has_unrecoverable_failures =
        has_post_failures || (has_confirmed_missing && !config.allow_incomplete_nzb);
    let files_str = || {
        entry_paths
            .iter()
            .map(|p| format!("\"{}\"", p.display()))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let resume_flags_str = || resume_flags_string(config);
    if has_post_failures {
        let n = outcome.failed_tasks.len();
        eprintln!();
        eprintln!("error: {n} segment(s) could not be posted after all retries.");
        eprintln!("The NZB will NOT be written — the upload is incomplete.");
        // Resume state is tracked for every run (not just ones started with
        // --resume) and persisted whenever a run ends incomplete like this
        // one — see `post_files_with_progress_and_cancel`'s final
        // persist-or-delete decision — so the segments that did succeed are
        // always recoverable here, regardless of whether --resume was
        // originally passed.
        if let Some(ref state_path) = resume_path {
            eprintln!();
            eprintln!("The successfully posted segments have been saved to:");
            eprintln!("  {}", state_path.display());
            eprintln!();
            eprintln!("To retry the missing segments and finish the upload, run:");
            eprintln!("  pesto {} --resume {}", files_str(), resume_flags_str());
        }
        eprintln!();
    }
    if has_confirmed_missing {
        let n = check_missing.len();
        eprintln!();
        if config.allow_incomplete_nzb {
            eprintln!(
                "warning: {n} article(s) still missing on the server after every repost \
                 attempt, including one final automatic recovery pass when the miss count \
                 was small enough."
            );
            eprintln!("Publishing anyway — --allow-incomplete-nzb was set.");
        } else {
            eprintln!(
                "error: {n} article(s) still missing on the server after every repost \
                 attempt, including one final automatic recovery pass when the miss count \
                 was small enough."
            );
            eprintln!(
                "The NZB will NOT be written — pass --allow-incomplete-nzb to publish anyway \
                 (e.g. relying on PAR2 recovery)."
            );
            // Same reasoning as the has_post_failures branch above: resume
            // state is always tracked and gets persisted here regardless of
            // whether --resume was passed to this run.
            if let Some(ref state_path) = resume_path {
                eprintln!();
                eprintln!(
                    "Or retry just the missing article(s) — the segments already \
                     confirmed present have been saved to:"
                );
                eprintln!("  {}", state_path.display());
                eprintln!("  pesto {} --resume {}", files_str(), resume_flags_str());
            }
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
            if outcome.failure_reason.is_some() {
                eprintln!("upload failed — skipping nzb output");
            } else {
                eprintln!("interrupted — skipping nzb output");
            }
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
                    tmdb_id: config.tmdb_id.clone(),
                    imdb_id: config.imdb_id.clone(),
                    tvdb_id: config.tvdb_id.clone(),
                    mal_id: config.mal_id.clone(),
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
                    // The server(s) that actually accepted an article this
                    // run (`outcome.servers`), not just the statically
                    // configured primary — a multi-server (failover) config
                    // commonly uses every configured server at once.
                    let history_servers_str = outcome.servers.join(", ");
                    pesto::history::record_upload(
                        &pesto::history::UploadRecord {
                            name: entry_label,
                            obfuscated_name: obf_name,
                            password: effective_password.as_deref(),
                            total_bytes,
                            // The group actually posted to (`pick_post_group`
                            // chose one at random from `config.groups`), not
                            // the configured list's static first entry.
                            group: outcome.groups.first().map(String::as_str),
                            server: (!history_servers_str.is_empty())
                                .then_some(history_servers_str.as_str()),
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
        // Reflects true completeness, independent of --allow-incomplete-nzb —
        // the notification should say "not fully ok" even when the user
        // chose to publish anyway.
        let had_failures =
            !outcome.failures.is_empty() || has_post_failures || has_confirmed_missing;
        pesto::notify::send_all(&pesto::notify::NotifyConfig {
            webhook_url: config.notify_webhook.as_deref(),
            ntfy_topic: config.notify_ntfy.as_deref(),
            name: entry_label,
            total_bytes,
            group: outcome.groups.first().map(String::as_str),
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
            // `nfo::generate` blocks on `bdinfo`/`mediainfo`, which can take
            // a while on a large Blu-ray disc — long enough that, with no
            // output in between, it looks like the process hung. Run it on
            // a blocking-pool thread and print a heartbeat every 10s so
            // there's always something on screen while it works.
            println!(
                "generating nfo (running bdinfo — this can take a while on large Blu-ray discs)..."
            );
            let nfo_paths = entry_paths.to_vec();
            let nfo_handle = tokio::task::spawn_blocking(move || pesto::nfo::generate(&nfo_paths));
            tokio::pin!(nfo_handle);
            let nfo_content = loop {
                tokio::select! {
                    res = &mut nfo_handle => break res.context("nfo generation task panicked")?,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                        println!("... still generating nfo, please wait");
                    }
                }
            };
            match nfo_content {
                Some(content) => match pesto::nfo::write(
                    nfo_out,
                    &format!("{}{content}", nfo_metadata_header(config)),
                ) {
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
        // Use `original_inputs`, not `inputs`: when --compress is active
        // `inputs` was replaced with the single compressed archive, which
        // would otherwise hide every original filename (and its extension)
        // from post-upload hooks — see the `original_inputs` snapshot above.
        let post_input_paths = original_inputs
            .iter()
            .map(|f| f.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let post_obfuscate = match config.obfuscate {
            ObfuscateMode::None => "none",
            ObfuscateMode::Full => "full",
            ObfuscateMode::FullShared => "full-shared",
            ObfuscateMode::Paranoid => "paranoid",
        };
        // PESTO_GROUP/PESTO_GROUPS report the group(s) actually posted to
        // (`outcome.groups`, chosen at random by `pick_post_group` from the
        // full configured list), not the static configured list itself —
        // this is a post-upload hook, so the real destination is known.
        let post_groups_str = outcome.groups.join(":");
        // Same reasoning for PESTO_SERVER/PESTO_SERVERS: report the
        // server(s) that actually accepted an article (`outcome.servers`),
        // not just the statically configured primary.
        let post_servers_str = outcome.servers.join(":");
        let post_tags_str = config.nzb_tags.join(" ");
        let hook_env = HookEnv {
            nzb_path: nzb_reported_path.as_deref(),
            nfo_path: nfo_path.as_deref(),
            name: entry_label,
            total_bytes,
            input_paths: &post_input_paths,
            group: outcome.groups.first().map(String::as_str),
            groups: &post_groups_str,
            password: effective_password.as_deref(),
            server: post_servers_str.split(':').next().unwrap_or(&config.host),
            servers: &post_servers_str,
            category: config.nzb_category.as_deref(),
            nzb_name: config.nzb_name.as_deref(),
            obfuscate: post_obfuscate,
            par2: config.par2,
            tags: &post_tags_str,
            tmdb_id: config.tmdb_id.as_deref(),
            imdb_id: config.imdb_id.as_deref(),
            tvdb_id: config.tvdb_id.as_deref(),
            mal_id: config.mal_id.as_deref(),
        };

        run_all_hooks(config, &hook_env);
    }

    // Cleanup temp dirs.
    if let Some(dir) = compress_temp_dir {
        let _ = std::fs::remove_dir_all(&dir);
    }
    // Only now — after the --check repost pass and the end-of-run failed-task
    // retry above have both had every chance to re-read a PAR2 file's bytes —
    // is it safe to remove the PAR2 temp dir. See `par2_temp_dir`'s doc
    // comment for why this used to happen too early.
    if !config.par2_only {
        let _ = tokio::fs::remove_dir_all(&outcome.par2_temp_dir).await;
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
        // show's slug.
        header.push_str(&format!(
            "TVDB : https://thetvdb.com/dereferrer/series/{tvdb_id}\n"
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

/// Whether `path`'s extension is one of `ext_filter` (case-insensitive). An
/// empty `ext_filter` matches everything (the `--ext` default: no filtering).
fn matches_ext_filter(path: &Path, ext_filter: &[String]) -> bool {
    if ext_filter.is_empty() {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            ext_filter
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(ext))
        })
}

/// Apply `--ext` to an already-expanded input list, in place. A no-op when
/// `ext_filter` is empty. Errors out if the filter drops every input, so a
/// mistyped extension (or an entry that is 100% subtitles/extras) fails
/// loudly instead of silently posting nothing.
fn apply_ext_filter(
    inputs: &mut Vec<pesto::walk::InputFile>,
    ext_filter: &[String],
    entry_label: &str,
) -> Result<()> {
    if ext_filter.is_empty() {
        return Ok(());
    }
    inputs.retain(|f| matches_ext_filter(&f.path, ext_filter));
    if inputs.is_empty() {
        anyhow::bail!(
            "no files matching --ext {} found in `{entry_label}`",
            ext_filter.join(",")
        );
    }
    Ok(())
}

/// Enumerate top-level entries of `dir` (files and subdirectories), sorted by
/// name using natural lexical ordering (so `E02` comes before `E10`).
///
/// `ext_filter` (from `--ext`) drops non-matching *files*; subdirectories are
/// always kept regardless of their name, since matching files may live inside
/// them.
fn top_level_entries(dir: &Path, ext_filter: &[String]) -> Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory `{}`", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| !is_artifact_entry(p))
        .filter(|p| p.is_dir() || matches_ext_filter(p, ext_filter))
        .collect();
    entries.sort_by(|a, b| {
        lexical_sort::natural_lexical_cmp(&a.to_string_lossy(), &b.to_string_lossy())
    });
    Ok(entries)
}

/// Derive the path for a `--season` consolidated NZB for `entry`. Prefers
/// `explicit_out` (from `--out`) if given; otherwise names the file after
/// `entry` and places it under `nzb_dir` (from config), or the current
/// directory if unset.
fn derive_season_nzb_path(
    explicit_out: Option<&Path>,
    entry: &Path,
    nzb_dir: Option<&str>,
) -> PathBuf {
    if let Some(out) = explicit_out {
        return out.to_path_buf();
    }
    let name = entry
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "season".to_string());
    let stem = format!("{name}.nzb");
    match nzb_dir {
        Some(dir) => expand_tilde(dir).join(&stem),
        None => PathBuf::from(&stem),
    }
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
            entries.extend(top_level_entries(dir, &params.ext_filter)?);
        } else {
            // A plain file is its own "entry".
            entries.push(dir.clone());
        }
    }

    if entries.is_empty() {
        anyhow::bail!("no entries found to post");
    }

    // A season batch merges every entry's NZB into one at the end, so they
    // all need the *same* archive password — resolved once, up front, and
    // handed to every entry below. A plain --each batch has no such merge,
    // so leaving this `None` lets each entry resolve (and randomise) its
    // own password independently inside `run_single_upload` (issue #67).
    let season_password: Option<String> = season_nzb
        .is_some()
        .then(|| {
            resolve_entry_password(
                None,
                params.config.compress_password.as_deref(),
                params.archive_password_raw.as_deref(),
            )
        })
        .flatten();

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

    let total_entries = entries.len();
    let mut handles = Vec::new();
    for (entry_idx, entry) in entries.iter().enumerate() {
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
        let task_password = season_password.clone();
        let label = entry
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "entry".to_string());

        info!(
            entry = entry_idx + 1,
            total = total_entries,
            name = %label,
            "--each entry"
        );

        let handle = tokio::spawn(async move {
            let _permit = permit;
            if !params.json_mode {
                println!("\n── {} ──", label);
            }
            run_single_upload(
                &params,
                &[entry],
                &label,
                Some(&task_cancel),
                task_password.as_deref(),
            )
            .await
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

    info!(entries = total_entries, "--each complete");

    // Write consolidated season NZB (and matching .nfo + hooks) when requested.
    if let Some(season_path) = season_nzb {
        if any_cancelled {
            eprintln!("interrupted — skipping season nzb output");
        } else if !all_segments.is_empty() {
            info!(entries = total_entries, path = %season_path.display(), "season merge starting");
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
                    .or_else(|| season_password.clone()),
                category: config.nzb_category.clone(),
                tmdb_id: config.tmdb_id.clone(),
                imdb_id: config.imdb_id.clone(),
                tvdb_id: config.tvdb_id.clone(),
                mal_id: config.mal_id.clone(),
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
                    Some(content) => match pesto::nfo::write(
                        &nfo_out,
                        &format!("{}{content}", nfo_metadata_header(config)),
                    ) {
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
                .or_else(|| season_password.clone());
            let season_obfuscate = match config.obfuscate {
                ObfuscateMode::None => "none",
                ObfuscateMode::Full => "full",
                ObfuscateMode::FullShared => "full-shared",
                ObfuscateMode::Paranoid => "paranoid",
            };
            // The union of groups actually used across every episode in the
            // season (each picked its own at random — see `all_groups`
            // above), not the static configured list.
            let season_groups_str = all_groups.join(":");
            // Same reasoning for the server(s): the union of servers that
            // actually accepted an article across every episode, derived
            // from each segment's `server_idx`, not the static config.
            let season_server_list: Vec<_> = config.all_servers().collect();
            let mut season_server_idxs: Vec<usize> =
                all_segments.iter().map(|s| s.server_idx).collect();
            season_server_idxs.sort_unstable();
            season_server_idxs.dedup();
            let season_servers_str = season_server_idxs
                .into_iter()
                .filter_map(|idx| season_server_list.get(idx))
                .map(|s| s.host.as_str())
                .collect::<Vec<_>>()
                .join(":");
            let season_tags_str = config.nzb_tags.join(" ");
            let hook_env = HookEnv {
                nzb_path: Some(&season_path),
                nfo_path: nfo_path.as_deref(),
                name: &season_label,
                total_bytes,
                input_paths: "",
                group: all_groups.first().map(String::as_str),
                groups: &season_groups_str,
                password: effective_password.as_deref(),
                server: season_servers_str.split(':').next().unwrap_or(&config.host),
                servers: &season_servers_str,
                category: config.nzb_category.as_deref(),
                nzb_name: config.nzb_name.as_deref(),
                obfuscate: season_obfuscate,
                par2: config.par2,
                tags: &season_tags_str,
                tmdb_id: config.tmdb_id.as_deref(),
                imdb_id: config.imdb_id.as_deref(),
                tvdb_id: config.tvdb_id.as_deref(),
                mal_id: config.mal_id.as_deref(),
            };
            // Skip hooks for --dry-run / --par2-only: no real upload happened.
            if !config.dry_run && !config.par2_only {
                run_all_hooks(config, &hook_env);
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

/// `--each`/`--season` options that also apply to directories detected by
/// `--watch` (see `run_watch`).
struct WatchBatchOpts {
    each: bool,
    season: bool,
    explicit_out: Option<PathBuf>,
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
    batch_opts: WatchBatchOpts,
    cancel: Arc<AtomicBool>,
) -> Result<bool> {
    let WatchBatchOpts {
        each,
        season,
        explicit_out,
    } = batch_opts;
    use tokio::sync::mpsc;

    eprintln!(
        "watching {} (poll every {}s)",
        watch_dir.display(),
        poll_interval
    );

    // `done`: entries that have been successfully uploaded (or permanently failed).
    let mut done: HashSet<PathBuf> = HashSet::new();
    // Pre-populate done with whatever is already present so we don't re-post on startup.
    if let Ok(existing) = top_level_entries(watch_dir, &params.ext_filter) {
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

        let entries = match top_level_entries(watch_dir, &params.ext_filter) {
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
                    let explicit_out = explicit_out.clone();

                    tokio::spawn(async move {
                        let _permit = permit;
                        if !params.json_mode {
                            println!("\n── watch: {} ──", label);
                        }
                        // Directories are posted as one combined NZB by default. With
                        // --each/--season, split per top-level entry instead, reusing
                        // the same batch machinery --each/--season use outside --watch.
                        let (success, task_cancelled) = if (each || season) && entry.is_dir() {
                            let season_nzb = season.then(|| {
                                derive_season_nzb_path(
                                    explicit_out.as_deref(),
                                    &entry,
                                    params.config.nzb_dir.as_deref(),
                                )
                            });
                            match run_batch(
                                Arc::clone(&params),
                                std::slice::from_ref(&entry),
                                jobs,
                                season_nzb,
                                task_cancel.clone(),
                            )
                            .await
                            {
                                Ok((_segments, any_cancelled, any_failures)) => {
                                    (!any_cancelled && !any_failures, any_cancelled)
                                }
                                Err(e) => {
                                    eprintln!(
                                        "watch: upload failed for `{}`: {e:#}",
                                        entry.display()
                                    );
                                    (false, false)
                                }
                            }
                        } else {
                            match run_single_upload(
                                &params,
                                std::slice::from_ref(&entry),
                                &label,
                                Some(&task_cancel),
                                None,
                            )
                            .await
                            {
                                Ok(result) if result.cancelled => (false, true),
                                Ok(_) => (true, false),
                                Err(e) => {
                                    eprintln!(
                                        "watch: upload failed for `{}`: {e:#}",
                                        entry.display()
                                    );
                                    (false, false)
                                }
                            }
                        };
                        if success {
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
                        }
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
fn run_merge_season(dir: &Path, display_name: Option<&str>, nzb_tags: Vec<String>) -> Result<()> {
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
            tmdb_id: None,
            imdb_id: None,
            tvdb_id: None,
            mal_id: None,
            tags: nzb_tags.clone(),
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
        return run_merge_season(dir, cli.nzb_name.as_deref(), nzb_tags);
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
    if let Some(p) = &session_log {
        tracing::debug!(path = %p.display(), "session log");
    }

    // Fall back to the append-only plain renderer whenever verbose logs share
    // stderr with the panel — at *any* -v level, not just -vv: an INFO-level
    // `-v` run also writes connection/pool lines to stderr, which the panel's
    // cursor-movement redraws would shred (and be shredded by). If the user
    // redirected logs to a file with --log-file the panel can run safely.
    let logs_to_stderr = cli.verbose >= 1 && cli.log_file.is_none();

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
    // Derive a human-readable label from the first input path without any
    // blocking filesystem calls (no is_dir/stat in the async executor).
    // file_name() returns the last path component; we strip a known extension
    // Strip the extension only for known media file types. Release names that
    // contain dots (e.g. "Show.S01E01.720p.BluRay-Group") must not be trimmed
    // by file_stem(), which would drop everything after the last dot.
    const STRIP_EXTS: &[&str] = &[
        "mkv", "mp4", "avi", "ts", "m2ts", "mov", "wmv", "flv", "webm", "mpg", "mpeg", "vob",
        "iso", "nzb", "zip", "rar", "7z", "tar", "gz", "bz2", "cbz", "cbr", "pdf", "epub",
    ];
    let label = cli
        .files
        .first()
        .and_then(|p| p.file_name())
        .map(|s| {
            let name = s.to_string_lossy();
            let p = std::path::Path::new(s);
            match p.extension().and_then(|e| e.to_str()) {
                Some(ext) if STRIP_EXTS.contains(&ext.to_ascii_lowercase().as_str()) => {
                    p.file_stem().unwrap_or(s).to_string_lossy().into_owned()
                }
                _ => name.into_owned(),
            }
        })
        .unwrap_or_else(|| format!("{}", std::process::id()));
    let result = run_single_upload(&params, &cli.files, &label, Some(&cancel), None).await?;

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
            // Strip `depth - 1` trailing components (everything in `name`
            // after the top-level folder) to land on the top-level folder
            // itself, not its parent. `ancestors().nth(k)` strips `k`
            // trailing components, so `nth(depth)` was one level too high —
            // it landed on the folder's *parent*, which under `--watch`
            // silently pulled in sibling top-level entries (issue #67).
            input
                .path
                .ancestors()
                .nth(depth - 1)
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

/// Decide the obfuscated archive stem for a `--compress`+`--obfuscate` run:
/// reuse the value a compatible prior `--resume` run recorded, or generate a
/// fresh one and record it for a *future* `--resume` to find.
///
/// Only touches the resume-state file when `--resume` is passed. Doing this
/// unconditionally would conflict with
/// `poster::post_files_with_progress_and_cancel`'s own handling of the same
/// file: when `--resume` is *not* passed, that function deliberately starts
/// from a fresh, empty state (see issue #18 — trusting whatever happens to
/// be on disk without being asked is the exact hazard it guards against),
/// which would silently erase whatever this function wrote moments earlier.
/// The practical result is the same rule as everywhere else in resume
/// handling: a stem only survives into a later run when every run in the
/// chain, including the first, passes `--resume`.
fn reuse_or_generate_archive_stem(resume_path: Option<&Path>, config: &Config) -> String {
    if !config.resume {
        return pesto::article::obfuscated_name();
    }
    let Some(rp) = resume_path else {
        return pesto::article::obfuscated_name();
    };
    let fingerprint = pesto::resume::RunFingerprint {
        article_size: config.article_size as u64,
        obfuscate: config.obfuscate,
        compress_format: config.compress_format.clone(),
        par2_percent: config.par2,
        file_counter: config.file_counter,
    };
    let mut state = pesto::resume::ResumeState::load(rp).unwrap_or_default();
    // Normalizes the loaded state first: a fingerprint mismatch clears any
    // stale archive_stem (and segments/files) before we look at it, so an
    // incompatible prior run's name is never reused.
    state.validate_run(&fingerprint);
    if let Some(stem) = state.archive_stem() {
        return stem.to_string();
    }
    let stem = pesto::article::obfuscated_name();
    state.set_archive_stem(stem.clone());
    let _ = state.save(rp);
    stem
}

/// The posting flags that `resume::RunFingerprint` actually checks,
/// formatted for a copy-pasteable `--resume` retry command. A retry using
/// different values for any of these gets its resume state silently (and
/// safely) discarded by `validate_run` — printing them explicitly means a
/// copy-pasted retry command actually resumes instead of quietly re-posting
/// everything from scratch. Closes the gap issue #18 called out: "the
/// printed resume hint only suggests `pesto <file> --resume` and drops the
/// original flags".
fn resume_flags_string(config: &Config) -> String {
    let obfuscate = match config.obfuscate {
        ObfuscateMode::None => "none",
        ObfuscateMode::Full => "full",
        ObfuscateMode::FullShared => "full-shared",
        ObfuscateMode::Paranoid => "paranoid",
    };
    let mut flags = format!(
        "--article-size {} --obfuscate={obfuscate} --par2 {}",
        config.article_size, config.par2
    );
    if let Some(fmt) = &config.compress_format {
        flags.push_str(&format!(" --compress={fmt}"));
    }
    if config.file_counter {
        flags.push_str(" --file-counter");
    }
    flags
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
    /// Colon-separated list of all newsgroups.
    groups: &'a str,
    password: Option<&'a str>,
    /// The server that actually accepted at least one article this run (the
    /// first entry of `servers` below), not just the statically configured
    /// primary — see `servers` for why this can differ in a multi-server
    /// (failover) config.
    server: &'a str,
    /// Colon-separated list of every server that actually accepted at least
    /// one article this run, derived from `PostedSegment::server_idx` on the
    /// real posted results — not the configured list, which can include a
    /// failover server that never ended up receiving anything, or omit which
    /// one of several equally-configured servers a given run landed on.
    servers: &'a str,
    category: Option<&'a str>,
    nzb_name: Option<&'a str>,
    obfuscate: &'a str,
    par2: u8,
    /// Space-separated list of NZB tags (empty string when none).
    tags: &'a str,
    /// TMDb reference, e.g. `movie/12345` or `tv/12345` (`--tmdb`).
    tmdb_id: Option<&'a str>,
    /// IMDb ID, e.g. `tt1234567` (`--imdb-id`).
    imdb_id: Option<&'a str>,
    /// TheTVDB ID (`--tvdb-id`).
    tvdb_id: Option<&'a str>,
    /// MyAnimeList ID (`--mal-id`).
    mal_id: Option<&'a str>,
}

fn apply_hook_env(child: &mut std::process::Command, env: &HookEnv<'_>) {
    child.env("PESTO_NAME", env.name);
    child.env("PESTO_BYTES", env.total_bytes.to_string());
    child.env("PESTO_INPUT_PATHS", env.input_paths);
    child.env("PESTO_SERVER", env.server);
    child.env("PESTO_SERVERS", env.servers);
    child.env("PESTO_GROUP", env.group.unwrap_or(""));
    child.env("PESTO_GROUPS", env.groups);
    child.env("PESTO_PASSWORD", env.password.unwrap_or(""));
    child.env("PESTO_CATEGORY", env.category.unwrap_or(""));
    child.env("PESTO_NZB_NAME", env.nzb_name.unwrap_or(""));
    child.env("PESTO_OBFUSCATE", env.obfuscate);
    child.env("PESTO_PAR2", env.par2.to_string());
    child.env("PESTO_TAGS", env.tags);
    child.env("PESTO_TMDB_ID", env.tmdb_id.unwrap_or(""));
    child.env("PESTO_IMDB_ID", env.imdb_id.unwrap_or(""));
    child.env("PESTO_TVDB_ID", env.tvdb_id.unwrap_or(""));
    child.env("PESTO_MAL_ID", env.mal_id.unwrap_or(""));
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

/// Run every executable file in `pre_hooks_dir` as a pre-upload hook, sorted by name.
///
/// Each script must exit 0 to allow the upload to proceed. The first non-zero
/// exit aborts immediately — remaining scripts in the directory are skipped.
fn run_pre_hooks_dir(pre_hooks_dir: &std::path::Path, env: &HookEnv<'_>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(pre_hooks_dir) else {
        return Ok(());
    };
    let mut scripts: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_executable(p))
        .collect();
    scripts.sort();
    for script in &scripts {
        println!("running pre-hook: {}", script.display());
        let mut child = hook_script_command(script);
        apply_hook_env(&mut child, env);
        match child.status() {
            Ok(s) if s.success() => println!("  pre-hook exited ok"),
            Ok(s) => anyhow::bail!(
                "pre-hook {} exited with status {s} — upload aborted",
                script.display()
            ),
            Err(e) => anyhow::bail!(
                "pre-hook {} failed to start: {e} — upload aborted",
                script.display()
            ),
        }
    }
    Ok(())
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

/// Run `config.post_hooks`, then every executable script in the hooks
/// directory (skipped when `config.no_hooks` is set). Warns when a
/// `post_hooks` entry resolves to a script inside the hooks directory, since
/// it would then run a second time during the directory scan (issue #40).
fn run_all_hooks(config: &Config, env: &HookEnv<'_>) {
    let hooks_dir = pesto::config::config_dir().map(|d| d.join("hooks"));

    for cmd in &config.post_hooks {
        if !config.no_hooks {
            if let Some(dir) = &hooks_dir {
                if pesto::hooks::post_hook_targets_hooks_dir(cmd, dir) {
                    tracing::warn!(
                        cmd,
                        hooks_dir = %dir.display(),
                        "post_hooks entry targets a script inside the hooks directory; it will also be executed by the directory scan. Set no_hooks = true to suppress the directory scan, or move this script out of the hooks directory to rely on post_hooks alone."
                    );
                }
            }
        }
        run_post_hook(cmd, env);
    }

    if !config.no_hooks {
        if let Some(dir) = &hooks_dir {
            run_hooks_dir(dir, env);
        }
    }
}

/// Run every executable file in `hooks_dir`, sorted by name, skipping
/// disabled ones (see [`pesto::hooks::is_disabled`]).
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
        .filter(|p| p.is_file() && is_executable(p) && !pesto::hooks::is_disabled(p))
        .collect();
    scripts.sort();
    if !scripts.is_empty() {
        println!(
            "discovered {} hook script(s) in {}; running in alphabetical order",
            scripts.len(),
            hooks_dir.display()
        );
    }
    for script in &scripts {
        println!("running hook: {}", script.display());
        let mut child = hook_script_command(script);
        apply_hook_env(&mut child, env);
        match child.status() {
            Ok(s) if s.success() => println!("  hook exited ok"),
            Ok(s) => eprintln!("  hook exited with status {s}"),
            Err(e) => eprintln!("  hook failed to start: {e}"),
        }
    }
}

/// Build the command used to launch a pre/post hook script.
///
/// On Windows, `CreateProcess` can launch `.exe`/`.bat`/`.cmd` directly, but
/// has no knowledge of `.ps1` files (that association only exists in
/// `ShellExecute`/Explorer). Running a `.ps1` via `Command::new(path)` fails
/// with "%1 is not a valid Win32 application" (os error 193), so it must be
/// invoked through an explicit PowerShell executable with `-File`. See
/// [`pesto::hooks::windows_powershell_exe`] for which one.
#[cfg(windows)]
fn hook_script_command(path: &std::path::Path) -> std::process::Command {
    let is_ps1 = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("ps1"));
    if is_ps1 {
        let mut c = std::process::Command::new(pesto::hooks::windows_powershell_exe());
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
        c.arg(path);
        c
    } else {
        std::process::Command::new(path)
    }
}

#[cfg(not(windows))]
fn hook_script_command(path: &std::path::Path) -> std::process::Command {
    std::process::Command::new(path)
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
    use pesto::config::{FileConfig, Overrides};
    use pesto::walk::InputFile;

    fn test_config(
        article_size: usize,
        obfuscate: ObfuscateMode,
        compress_format: Option<&str>,
        par2: u8,
    ) -> Config {
        let mut file = FileConfig::default();
        file.server.host = Some("news.example.com".into());
        file.posting.groups = Some(vec!["alt.test".into()]);
        Config::resolve(
            file,
            Overrides {
                article_size: Some(article_size),
                obfuscate: Some(obfuscate),
                compress_format: compress_format.map(str::to_string),
                par2: Some(par2),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn resume_flags_string_includes_every_fingerprinted_flag() {
        let config = test_config(384_000, ObfuscateMode::Full, None, 10);
        assert_eq!(
            resume_flags_string(&config),
            "--article-size 384000 --obfuscate=full --par2 10"
        );
    }

    #[test]
    fn resume_flags_string_includes_compress_only_when_set() {
        let none_compressed = test_config(768_000, ObfuscateMode::None, None, 0);
        assert!(!resume_flags_string(&none_compressed).contains("--compress"));

        let compressed = test_config(768_000, ObfuscateMode::FullShared, Some("7z"), 5);
        assert_eq!(
            resume_flags_string(&compressed),
            // `file_counter` defaults to true for `full-shared` — see
            // `Config::resolve`'s obfuscate-mode-dependent default.
            "--article-size 768000 --obfuscate=full-shared --par2 5 --compress=7z --file-counter"
        );
    }

    // ── reuse_or_generate_archive_stem ─────────────────────────────────────

    #[test]
    fn archive_stem_without_resume_is_always_fresh_and_untracked() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = false;
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("release.pesto-state");

        let a = reuse_or_generate_archive_stem(Some(&state_path), &config);
        let b = reuse_or_generate_archive_stem(Some(&state_path), &config);

        assert_ne!(
            a, b,
            "without --resume, every call must generate a fresh name"
        );
        assert!(
            !state_path.exists(),
            "without --resume, nothing should be written to disk"
        );
    }

    #[test]
    fn archive_stem_without_a_resume_path_is_fresh() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = true;
        let a = reuse_or_generate_archive_stem(None, &config);
        let b = reuse_or_generate_archive_stem(None, &config);
        assert_ne!(a, b);
    }

    #[test]
    fn archive_stem_is_generated_and_recorded_on_first_resume_run() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = true;
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("release.pesto-state");

        let stem = reuse_or_generate_archive_stem(Some(&state_path), &config);

        let state = pesto::resume::ResumeState::load(&state_path).unwrap();
        assert_eq!(state.archive_stem(), Some(stem.as_str()));
    }

    #[test]
    fn archive_stem_is_reused_on_a_compatible_resume_run() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = true;
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("release.pesto-state");

        let first = reuse_or_generate_archive_stem(Some(&state_path), &config);
        let second = reuse_or_generate_archive_stem(Some(&state_path), &config);

        assert_eq!(
            first, second,
            "a compatible resume run must reuse the same stem"
        );
    }

    #[test]
    fn archive_stem_is_regenerated_when_posting_parameters_changed() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = true;
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("release.pesto-state");

        let first = reuse_or_generate_archive_stem(Some(&state_path), &config);

        // A later run using a different --article-size: the old stem
        // (recorded under a now-mismatched fingerprint) must not be reused.
        config.article_size = 384_000;
        let second = reuse_or_generate_archive_stem(Some(&state_path), &config);

        assert_ne!(
            first, second,
            "a fingerprint mismatch must not reuse the old stem"
        );
        let state = pesto::resume::ResumeState::load(&state_path).unwrap();
        assert_eq!(state.archive_stem(), Some(second.as_str()));
    }

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
            vec![PathBuf::from("/media/Show")]
        );
    }

    #[test]
    fn collect_compress_roots_nested_subfolder_strips_to_top_level() {
        // Regression test for issue #67: a file nested two levels deep
        // inside the top-level folder (e.g. `Test1/Subs/en.srt`) must still
        // resolve to `Test1`, not to `Test1`'s parent.
        let files = vec![InputFile {
            path: PathBuf::from("/home/user/upload/Test1/Subs/en.srt"),
            name: "Test1/Subs/en.srt".to_string(),
        }];
        assert_eq!(
            collect_compress_roots(&files),
            vec![PathBuf::from("/home/user/upload/Test1")]
        );
    }

    #[test]
    fn collect_compress_roots_relative_folder_resolves_to_folder_itself() {
        // A directory passed with a bare relative path (e.g. `pesto Test1
        // --compress` run from Test1's parent) must still resolve to
        // `Test1`, not fall back to per-file roots or an empty path.
        let files = vec![
            InputFile {
                path: PathBuf::from("Test1/movie.mkv"),
                name: "Test1/movie.mkv".to_string(),
            },
            InputFile {
                path: PathBuf::from("Test1/movie.nfo"),
                name: "Test1/movie.nfo".to_string(),
            },
        ];
        assert_eq!(collect_compress_roots(&files), vec![PathBuf::from("Test1")]);
    }

    #[test]
    fn collect_compress_roots_does_not_leak_sibling_top_level_folders() {
        // Regression test for issue #67: compressing `Test1` under
        // `--watch` must never resolve to the watch directory itself, or
        // sibling entries like `Test2` end up bundled into the same
        // archive.
        let files = vec![
            InputFile {
                path: PathBuf::from("/home/user/upload/Test1/movie.mkv"),
                name: "Test1/movie.mkv".to_string(),
            },
            InputFile {
                path: PathBuf::from("/home/user/upload/Test1/movie.nfo"),
                name: "Test1/movie.nfo".to_string(),
            },
        ];
        let roots = collect_compress_roots(&files);
        assert_eq!(roots, vec![PathBuf::from("/home/user/upload/Test1")]);
        assert!(!roots.contains(&PathBuf::from("/home/user/upload")));
    }

    #[test]
    fn resolve_entry_password_no_flag_is_no_password() {
        assert_eq!(resolve_entry_password(None, None, None), None);
    }

    #[test]
    fn resolve_entry_password_explicit_password_is_reused_verbatim() {
        // `--password mypass`: same literal string every time it's resolved,
        // matching every entry under --each/--season/--watch sharing it.
        for raw in [None, Some(""), Some("mypass")] {
            assert_eq!(
                resolve_entry_password(None, Some("mypass"), raw),
                Some("mypass".to_string())
            );
        }
    }

    #[test]
    fn resolve_entry_password_bare_flag_generates_a_password() {
        // Regression for issue #67: bare `--password` (raw == Some("")) with
        // no forced/explicit password must still produce something to
        // protect the archive with.
        let pw = resolve_entry_password(None, None, Some(""));
        assert!(pw.is_some());
        assert_eq!(pw.as_deref().map(str::len), Some(24));
    }

    #[test]
    fn resolve_entry_password_bare_flag_is_unique_per_call() {
        // Regression for issue #67: under plain --each/--watch (no forced
        // password), every call must mint its own password instead of the
        // whole run sharing one — this is what let `Test1.nzb` and
        // `Test2.nzb` end up with the identical password after the
        // `Cli::overrides()`-time resolution used to bake one value in for
        // the whole process.
        let a = resolve_entry_password(None, None, Some(""));
        let b = resolve_entry_password(None, None, Some(""));
        assert_ne!(a, b);
    }

    #[test]
    fn resolve_entry_password_forced_wins_over_explicit_and_bare() {
        // Regression for issue #67: a --season batch resolves one shared
        // password up front (`run_batch`'s `season_password`) and forces it
        // on every entry — every episode must get that exact value even
        // though each entry, left alone, would otherwise resolve its own
        // (explicit or freshly-random) password.
        assert_eq!(
            resolve_entry_password(Some("season-pw"), Some("explicit-pw"), Some("")),
            Some("season-pw".to_string())
        );
        assert_eq!(
            resolve_entry_password(Some("season-pw"), None, Some("")),
            Some("season-pw".to_string())
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

        let names: Vec<String> = top_level_entries(&dir, &[])
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["ep01.mkv"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn matches_ext_filter_is_case_insensitive_and_empty_means_everything() {
        assert!(matches_ext_filter(Path::new("Show.MKV"), &["mkv".into()]));
        assert!(matches_ext_filter(Path::new("Show.mkv"), &["MKV".into()]));
        assert!(!matches_ext_filter(Path::new("Show.srt"), &["mkv".into()]));
        assert!(matches_ext_filter(Path::new("Show.srt"), &[]));
        assert!(!matches_ext_filter(Path::new("Show"), &["mkv".into()]));
    }

    #[test]
    fn top_level_entries_filters_loose_files_by_ext_but_keeps_directories() {
        let dir = std::env::temp_dir().join(format!(
            "pesto_each_ext_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("Extras")).unwrap();
        std::fs::write(dir.join("ep01.mkv"), b"x").unwrap();
        std::fs::write(dir.join("ep01.srt"), b"x").unwrap();

        let names: Vec<String> = top_level_entries(&dir, &["mkv".to_string()])
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // The loose .srt sibling is dropped; the subdirectory is kept even
        // though "Extras" has no matching extension of its own, since a
        // matching file could live inside it.
        assert_eq!(names, ["ep01.mkv", "Extras"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_ext_filter_drops_non_matching_and_errors_when_nothing_left() {
        let mut inputs = vec![
            pesto::walk::InputFile {
                path: PathBuf::from("ep01.mkv"),
                name: "ep01.mkv".to_string(),
            },
            pesto::walk::InputFile {
                path: PathBuf::from("ep01.srt"),
                name: "ep01.srt".to_string(),
            },
        ];
        apply_ext_filter(&mut inputs, &["mkv".to_string()], "entry").unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].name, "ep01.mkv");

        let mut only_subs = vec![pesto::walk::InputFile {
            path: PathBuf::from("ep01.srt"),
            name: "ep01.srt".to_string(),
        }];
        assert!(apply_ext_filter(&mut only_subs, &["mkv".to_string()], "entry").is_err());

        // Empty filter is a no-op.
        let mut untouched = vec![pesto::walk::InputFile {
            path: PathBuf::from("ep01.srt"),
            name: "ep01.srt".to_string(),
        }];
        apply_ext_filter(&mut untouched, &[], "entry").unwrap();
        assert_eq!(untouched.len(), 1);
    }

    #[test]
    fn derive_season_nzb_path_prefers_explicit_out() {
        let path = derive_season_nzb_path(
            Some(Path::new("/custom/out.nzb")),
            Path::new("/downloads/Show.S01"),
            Some("/nzbs"),
        );
        assert_eq!(path, PathBuf::from("/custom/out.nzb"));
    }

    #[test]
    fn derive_season_nzb_path_names_after_entry_under_nzb_dir() {
        let path = derive_season_nzb_path(None, Path::new("/downloads/Show.S01"), Some("/nzbs"));
        assert_eq!(path, PathBuf::from("/nzbs/Show.S01.nzb"));
    }

    #[test]
    fn derive_season_nzb_path_falls_back_to_cwd_relative_name() {
        let path = derive_season_nzb_path(None, Path::new("/downloads/Show.S01"), None);
        assert_eq!(path, PathBuf::from("Show.S01.nzb"));
    }
}
