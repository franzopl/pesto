//! Command-line declarations and conversion to configuration overrides.

use std::path::PathBuf;

use clap::Parser;
use parmesan::SimdPath;
use pesto::config::{ObfuscateMode, Par2SliceSize};

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

Any config value can be overridden by the matching flag below.

Ctrl-C or SIGTERM once stops gracefully; press it again to abort immediately.
Progress is saved for --resume in either case.";

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
  pesto --cleanup movie.mkv       delete source after successful upload
  pesto --cleanup-to ./archive/   move sources to ./archive/ after upload
  pesto --watch ./in/ --cleanup   watch and auto-delete after upload

By default pesto posts under a freshly generated random identity. Set
[posting].from (or --from) only if you need a fixed one.";

#[derive(Parser, Debug)]
#[command(
    name = "pesto",
    version = pesto::DISPLAY_VERSION,
    about = ABOUT,
    long_about = LONG_ABOUT,
    after_help = AFTER_HELP
)]
pub(super) struct Cli {
    /// TOML config file to load. With no value (`pesto --config`), launch the
    /// interactive setup wizard instead. When omitted, the default config
    /// path is used if it exists.
    #[arg(short, long, value_name = "PATH", num_args = 0..=1)]
    pub(super) config: Option<Option<PathBuf>>,

    /// Download the latest pesto-v* release from GitHub for this platform,
    /// verify its checksum, and replace the running binary with it. Exits
    /// without touching anything else (no upload, no config load).
    #[arg(long)]
    pub(super) update: bool,

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

    /// Route NNTP connections through a SOCKS5 proxy; only SOCKS5 is supported.
    #[arg(short = 'p', long, value_name = "URL")]
    proxy: Option<String>,
    /// Display the public exit IP through --proxy before posting (contacts api.ipify.org).
    #[arg(long)]
    proxy_check_ip: bool,

    /// Authentication username [config: auth.username].
    #[arg(short = 'u', long, value_name = "USER")]
    username: Option<String>,

    /// Authentication password for the NNTP server [config: auth.password].
    #[arg(long = "auth-password", value_name = "PASS")]
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
    pub(super) out: Option<PathBuf>,

    /// Directory where `.nzb` files are saved; filename derived from upload
    /// name [config: output.nzb_dir]. Overridden by --out.
    #[arg(long, value_name = "DIR")]
    nzb_dir: Option<PathBuf>,

    /// Obfuscation mode: `none`, `full`, `full-shared`, or `light`. A bare
    /// `--obfuscate` means `full`. `full-shared` reuses one random name
    /// across every file in the release (archive + PAR2 volumes) so
    /// indexers can still group them; `light` is the same, but the yEnc
    /// `name=` matches the Subject exactly instead of adding its own random
    /// suffix, for indexers that key grouping off that exact match
    /// [config: posting.obfuscate, default none].
    #[arg(long, value_name = "MODE", value_enum, num_args = 0..=1,
          default_missing_value = "full", require_equals = true)]
    obfuscate: Option<ObfuscateMode>,

    /// Percentage of PAR2 recovery data to generate; 0 disables it
    /// [config: posting.par2, default 10].
    #[arg(long, value_name = "PERCENT")]
    par2: Option<u8>,

    /// PAR2 slice size: `auto` (whole multiples of the article size, see
    /// --par2-effort), `article` (one slice per article: most protection,
    /// most CPU) or a fixed size such as "4 MiB"
    /// [config: posting.par2_slice_size, default auto].
    #[arg(long, value_name = "SIZE")]
    slice_size: Option<Par2SliceSize>,

    /// Scale the CPU budget of automatic PAR2 slicing: 2 doubles the slices,
    /// the scattered article loss large releases survive and the PAR2
    /// compute time; 0.5 halves them [config: posting.par2_effort, default 1].
    #[arg(long, value_name = "FACTOR")]
    par2_effort: Option<f64>,

    /// Target number of PAR2 input slices [default: auto].
    #[arg(long, value_name = "N")]
    slice_count: Option<usize>,

    /// Exact number of PAR2 recovery blocks to generate [default: auto].
    #[arg(long, value_name = "N")]
    recovery_count: Option<usize>,

    /// Maximum RAM for PAR2 recovery buffers specifically, e.g. "512 MiB"
    /// [config: posting.par2_memory_limit, default "1 GiB"]. For the whole
    /// process's budget, see --memory-limit.
    #[arg(long, value_name = "SIZE")]
    par2_memory_limit: Option<String>,

    /// Global memory budget for the whole process (PAR2, uploads, check
    /// queue together), not just PAR2: an absolute size ("8 GiB"), a
    /// percentage of host RAM ("70%"), or "auto" (default) to derive it from
    /// RLIMIT_AS/cgroup/host RAM with no explicit override. PAR2 buffers plus
    /// flush queue draw a 60% share of what is left of it, capped by
    /// --par2-memory-limit and the RLIMIT_AS-specific pass-sizing model
    /// [config: posting.memory_limit, default "auto"].
    #[arg(long, value_name = "SIZE|PCT|auto")]
    memory_limit: Option<String>,

    /// Log address-space and RSS usage once a second, tagged with the stage
    /// of the run. The peak and the stage it occurred in are always reported
    /// at exit; this adds the full trace, for diagnosing *when* a run grows.
    #[arg(long)]
    pub(super) memory_trace: bool,

    /// Print a detailed memory report at exit: the effective ceiling
    /// (address-space/cgroup/host, whichever is tightest), the gap between
    /// live heap data and VmSize (allocator/VA overhead), and the worst
    /// pressure level reached during the run.
    #[arg(long)]
    pub(super) memory_report: bool,

    /// Base for a per-run directory holding intermediate PAR2 files. Recovery
    /// is computed in RAM first; the directory is created when the PAR2 files
    /// are materialised and removed after posting/checks/retries. Use -v to
    /// see its effective path. Defaults to the OS temp directory (e.g. /tmp).
    /// Ignored with --par2-only, which writes PAR2 files next to the sources
    /// instead [config: posting.par2_temp_dir].
    #[arg(long, value_name = "DIR")]
    par2_temp_dir: Option<String>,

    /// Number of threads for parallel PAR2 compute; 0 means one per usable
    /// physical core, at most 16 [config: posting.threads, default 0].
    #[arg(long, value_name = "N")]
    threads: Option<usize>,

    /// Force a SIMD backend for PAR2 [config: posting.simd, default auto].
    #[arg(long, value_enum, value_name = "MODE")]
    simd: Option<SimdPath>,

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

    /// Split the archive built by --compress into multiple volumes instead
    /// of one monolithic file, e.g. `500m` or `4g`. Supported with
    /// `--compress=rar` and `--compress=7z`; rejected with `--compress=zip`
    /// (7z's zip backend has no volume support)
    /// [config: compression.volume_size].
    #[arg(long, value_name = "SIZE")]
    compress_volume_size: Option<String>,

    /// Bundle files into a password-protected archive before posting. Optional
    /// PASSWORD: bare `--password` generates a random 24-character password
    /// and prints it; `--password=mypass` uses an explicit one. Implies
    /// `--compress` with the configured or default format.
    #[arg(long = "password", value_name = "PASSWORD",
          num_args = 0..=1, default_missing_value = "")]
    pub(super) archive_password: Option<String>,

    /// Friendly display name emitted as `<meta type="title">` in the `.nzb`
    /// (shown by NZBGet / SABnzbd) [config: output.nzb_title].
    #[arg(long, value_name = "NAME")]
    pub(super) nzb_title: Option<String>,

    /// Deprecated alias of `--nzb-title`; still works, but prefer
    /// `--nzb-title` in new scripts. Will stop being accepted in a future
    /// release.
    #[arg(long, value_name = "NAME", hide = true)]
    pub(super) nzb_name: Option<String>,

    /// Extraction password written to `<meta type="password">` in the `.nzb`;
    /// defaults to the archive password when `--password` is set
    /// [config: output.nzb_password].
    #[arg(long, value_name = "PASS")]
    nzb_password: Option<String>,

    /// Encrypt articles using yEnc body and control-line encryption. Optional
    /// PASSWORD: bare `--encrypt` generates a random 24-character password
    /// and prints it; `--encrypt=<password>` uses an explicit one
    /// [config: encryption.password].
    #[arg(
        long,
        value_name = "PASSWORD",
        num_args = 0..=1,
        default_missing_value = "",
        require_equals = true
    )]
    pub(super) encrypt: Option<String>,

    /// Category written to `<meta type="category">` in the `.nzb`
    /// [config: output.nzb_category].
    #[arg(long, value_name = "CAT")]
    nzb_category: Option<String>,

    /// Tag written to `<meta type="tag">` in the `.nzb`; can be repeated
    /// multiple times [config: output.nzb_tags].
    /// When this flag is used on the command line, it replaces any tags set in
    /// the config file (they are not merged).
    #[arg(long, value_name = "TAG", action = clap::ArgAction::Append)]
    pub(super) nzb_tag: Vec<String>,

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

    /// TheTVDB reference written to `<meta type="tvdbid">` in the `.nzb`, as
    /// `movie/<id>` or `series/<id>` (`movie:<id>` / `series:<id>`, and
    /// `tv/<id>` as an alias for `series/<id>`, also accepted). A bare
    /// numeric ID (e.g. `81189`) is still accepted and defaults to `series`.
    /// When `--nzb-category` and `--tmdb` are both unset, the category
    /// defaults to `movies` or `tv` accordingly. Also added as a line in the
    /// `.nfo` when `--nfo` is set, linking to the right TheTVDB dereferrer
    /// (`/dereferrer/movie/<id>` or `/dereferrer/series/<id>`). Aliased as
    /// `--tvdb`.
    #[arg(long, alias = "tvdb", value_name = "ID")]
    tvdb_id: Option<String>,

    /// MyAnimeList ID written to `<meta type="malid">` in the `.nzb`, e.g.
    /// `1535`. Also added as a line in the `.nfo` when `--nfo` is set.
    /// Aliased as `--mal`.
    #[arg(long, alias = "mal", value_name = "ID")]
    mal_id: Option<String>,

    /// `Date:` header for each article: `now` (current time), deprecated
    /// `random` (within the last 2 hours), or a fixed RFC 2822 timestamp.
    /// Omit to let the server supply the date [config: posting.date].
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
    /// none`, `full-shared` and `light`, which already accept cross-file
    /// correlation by wire metadata as part of their own design (bare
    /// filename, or a shared prefix/From); off by default for
    /// `full`/`article`, where an explicit counter is rejected because it
    /// contradicts the mode contract. Pass
    /// --no-file-counter to force it off regardless of mode. See
    /// `ROADMAP.md` "Subject file counter" [config: posting.file_counter].
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
    pub(super) output_format: String,

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
    /// `PESTO_CATEGORY`, `PESTO_NZB_TITLE`, `PESTO_OBFUSCATE`, `PESTO_PAR2`,
    /// `PESTO_TAGS`
    /// Can be specified multiple times. [config: output.pre_hooks].
    #[arg(long, value_name = "CMD", action = clap::ArgAction::Append)]
    pre_hook: Vec<String>,

    /// Shell command to execute after each successful upload. The command
    /// receives upload details via environment variables:
    /// `PESTO_NZB`, `PESTO_NFO`, `PESTO_NAME`, `PESTO_BYTES`,
    /// `PESTO_INPUT_PATHS`, `PESTO_GROUP`, `PESTO_GROUPS`, `PESTO_PASSWORD`,
    /// `PESTO_SERVER`, `PESTO_SERVERS`, `PESTO_CATEGORY`, `PESTO_NZB_TITLE`,
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
    pub(super) quiet: bool,

    /// Ring the terminal bell on completion [config: output.bell].
    #[arg(long)]
    pub(super) bell: bool,

    /// Treat each top-level entry in a directory argument as an independent
    /// upload with its own NZB. PAR2 and NZB naming follow the entry name.
    /// Combine with --jobs for parallel uploads. Also applies to directories
    /// detected by --watch: each one is split per top-level entry instead of
    /// posted as a single combined NZB.
    #[arg(long)]
    pub(super) each: bool,

    /// Like --each, but also produces one consolidated NZB for the whole
    /// directory. The consolidated NZB is named after the directory. Also
    /// applies to directories detected by --watch.
    #[arg(long)]
    pub(super) season: bool,

    /// Number of independent uploads to run in parallel when --each or
    /// --season is active. Default 1 (sequential). 0 means one per logical CPU.
    #[arg(long, value_name = "N", default_value = "1")]
    pub(super) jobs: usize,

    /// Restrict uploads to files with one of these extensions
    /// (comma-separated, case-insensitive, without the dot: `--ext mkv,mp4`).
    /// A directory argument is still walked as usual; only files that don't
    /// match are dropped. Most useful with --each/--season/--watch to skip
    /// subtitle, sample, or other extra files bundled next to the video.
    /// Default: no filtering (every file is included).
    #[arg(long, value_name = "EXT", value_delimiter = ',')]
    pub(super) ext: Vec<String>,

    /// Exclude directory entries matching GLOB (repeatable, case-sensitive).
    /// Basename globs match at any depth; paths are relative to the input root.
    /// Adds to built-in OS/FUSE exclusions. Explicit files bypass exclusions.
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,

    /// Disable default and custom directory-entry exclusions.
    #[arg(long)]
    no_exclude: bool,

    /// Watch DIR for new entries and post each one automatically. A directory
    /// entry is posted as a single combined NZB by default, or split per
    /// top-level entry (one NZB per file) when --each or --season is also
    /// passed. On completion each entry is moved to --watch-done (if set);
    /// otherwise it is left in place.
    /// Exits cleanly on SIGTERM / Ctrl-C after finishing any in-progress upload.
    #[arg(long, value_name = "DIR")]
    pub(super) watch: Option<PathBuf>,

    /// Destination directory for entries processed by --watch. When omitted,
    /// completed entries are left in place.
    #[arg(long, value_name = "DIR")]
    pub(super) watch_done: Option<PathBuf>,

    /// Delete successfully uploaded sources (files or directories) instead of
    /// leaving them in place. Works with any upload mode (--watch, --each,
    /// --season, or direct file upload). Mutually exclusive with --cleanup-to.
    #[arg(long)]
    pub(super) cleanup: bool,

    /// Move successfully uploaded sources to DIR instead of leaving them in
    /// place. Works with any upload mode. Mutually exclusive with --cleanup.
    #[arg(long, value_name = "DIR")]
    pub(super) cleanup_to: Option<PathBuf>,

    /// How often (in seconds) to poll the watched directory for new entries
    /// [default: 30].
    #[arg(long, value_name = "SECS", default_value = "30")]
    pub(super) watch_interval: u64,

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
    pub(super) stdin_name: Option<String>,

    /// Increase log verbosity. Repeat for more detail:
    ///   `-v` = INFO (worker state, file discovery, PAR2 geometry),
    ///   `-vv` = DEBUG (NNTP commands and responses — credentials masked),
    ///   `-vvv` = TRACE (fine-grained timing and buffer events).
    /// Logs are written to stderr (or --log-file). `RUST_LOG` overrides the
    /// level when set.
    #[arg(short, long, action = clap::ArgAction::Count, value_name = "LEVEL")]
    pub(super) verbose: u8,

    /// Redirect verbose log output to FILE instead of stderr. The terminal
    /// progress panel is kept active when this flag is set. Has no effect
    /// without -v.
    #[arg(long, value_name = "FILE")]
    pub(super) log_file: Option<PathBuf>,

    /// Disable the per-upload DEBUG log normally saved under
    /// `<history_dir>/logs/` [config: output.session_log, default on].
    #[arg(long)]
    pub(super) no_session_log: bool,

    /// Merge all per-episode NZBs in DIR into one combined season NZB and exit.
    /// No server connection is required. NZBs are grouped by their season
    /// identifier (e.g. `S02`); each group produces one output NZB written
    /// beside the source files. Use `--nzb-title` to override the display name
    /// in the NZB `<head>`.
    #[arg(long, value_name = "DIR", conflicts_with = "files")]
    pub(super) merge_season: Option<PathBuf>,

    /// Files or directories to post. A directory is walked recursively and
    /// every file inside it is posted, keeping the folder structure.
    /// Use `-` to read from stdin (requires --stdin-name).
    #[arg(value_name = "PATH")]
    pub(super) files: Vec<PathBuf>,
}

mod overrides;

#[cfg(test)]
mod tests;
