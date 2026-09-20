use std::path::PathBuf;

use clap::{Parser, Subcommand};
use penne::check::CheckMethod;
use penne::config::ProcessingMode;

#[derive(Parser)]
#[command(
    name = "penne",
    version = penne::DISPLAY_VERSION,
    about = "Fast NZB downloader",
    long_about = "Fast NZB downloader.\n\n\
Server credentials are read from a TOML config file. If --config is not \
given, penne loads it from the OS-standard location: $XDG_CONFIG_HOME/penne/config.toml \
(or, failing that, ~/.config/penne/config.toml) on Linux/macOS, or \
%APPDATA%\\penne\\config.toml on Windows. Create that file interactively \
with `penne --config`, or point at a specific file with `--config <FILE>`.",
after_help = "QUICK START:\n  penne --config                         Configure a news server\n  penne check RELEASE.nzb                Verify availability without downloading\n  penne check RELEASE.nzb --fail-fast -q Stop at the first confirmed miss\n  penne download RELEASE.nzb             Download, repair, and extract\n\n\
Run `penne <command> --help` (or `penne help <command>`) for command options."
)]
pub(super) struct Cli {
    #[command(subcommand)]
    pub(super) command: Option<Command>,

    /// TOML config file (server credentials, download directory). With no
    /// value (`penne --config`), launch the interactive setup wizard
    /// instead of running a command. When omitted entirely, the default
    /// config path is used.
    #[arg(long, global = true)]
    pub(super) config: Option<Option<PathBuf>>,

    /// Increase log verbosity. Repeat for more detail:
    ///   `-v` = INFO (server selection, mode, PAR2/extract decisions),
    ///   `-vv` = DEBUG (NNTP commands and responses — credentials masked),
    ///   `-vvv` = TRACE (fine-grained timing and buffer events).
    /// Logs are written to stderr (or --log-file). `RUST_LOG` overrides the
    /// level when set. Matches `pesto`'s `-v`/`--verbose` convention.
    #[arg(short, long, action = clap::ArgAction::Count, global = true, value_name = "LEVEL")]
    pub(super) verbose: u8,

    /// Redirect verbose log output to FILE instead of stderr. Has no effect
    /// without -v.
    #[arg(long, global = true, value_name = "FILE")]
    pub(super) log_file: Option<PathBuf>,
}

#[derive(Subcommand)]
pub(super) enum Command {
    #[command(about = "Inspect an NZB's files, segments, and size")]
    /// Parse a `.nzb` and print file/segment/size counts.
    Info {
        /// Path to the `.nzb` file.
        nzb: PathBuf,
    },
    #[command(about = "Download, repair, and extract one or more NZBs")]
    /// Download and assemble the contents of one or more `.nzb` files. Exits
    /// 0 if every file ended up complete with no repair needed, 1 if PAR2
    /// repaired something but the end result is complete, 2 if data is still
    /// missing or damaged (PAR2 couldn't fix it, no recovery data was
    /// available, or repair was skipped via `--mode download`), 3 on a fatal
    /// error (config, network, I/O). `--stat`'s own pass/fail (see below)
    /// surfaces as a fatal error too, since it never reaches the
    /// download/repair pipeline these codes describe.
    ///
    /// Multiple `.nzb` files download sequentially, sharing one `--config`/
    /// `--out-dir`/`--mode`/etc. for the whole batch; the overall exit code
    /// is the worst (highest) of any individual file's own code — one
    /// incomplete release in a batch of ten still needs to fail the run.
    /// Each release beyond the first downloads into its own subdirectory
    /// (named after its `.nzb` file's stem) under the shared destination, so
    /// same-named files across releases can never collide; a single `.nzb`
    /// keeps downloading straight into the destination, unchanged from
    /// before this flag accepted more than one path.
    Download {
        /// Path(s) to the `.nzb` file(s).
        #[arg(required = true)]
        nzb: Vec<PathBuf>,
        /// Destination directory for completed files. Defaults to the
        /// config file's `download_dir`, or the current directory.
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Archive extraction password. Overrides the `.nzb`'s own
        /// `<meta type="password">`, if any — useful for obfuscated
        /// releases that don't carry the password in the `.nzb` itself.
        #[arg(long)]
        password: Option<String>,
        /// Only check that every segment is still present on the
        /// configured server(s) — no download, decode, PAR2, or
        /// extraction. Three methods, from cheapest-but-least-trustworthy
        /// to most expensive-but-certain: `stat` (the default when the
        /// flag is given with no value — RFC 3977 §6.2.4, a bare
        /// existence check against the server's index), `head` (RFC 3977
        /// §6.2.2 — still cheap, but reads from the same article storage
        /// `BODY` does, catching a provider whose `STAT` index has
        /// drifted out of sync with what it can actually deliver), or
        /// `body` (a full real fetch, discarded — maximum certainty, real
        /// bandwidth cost, no different from an actual download of the
        /// same segment).
        #[arg(long, value_enum, value_name = "METHOD")]
        stat: Option<Option<CheckMethod>>,
        /// Only meaningful with `--stat`: check `N` segment(s) of each file,
        /// spread evenly across it, instead of every segment in the
        /// release. Most useful with `--stat=body`, whose per-segment cost
        /// is a real article fetch — checking a whole large release that
        /// way often isn't worth it, but a small, protocol-normal sample
        /// (read to completion, connection closed cleanly — never an
        /// abandoned mid-transfer read, which real NNTP servers'
        /// anti-abuse systems tend to flag) still catches a provider whose
        /// article storage doesn't back up what it claims, wherever in the
        /// file that shows up — not just at the start. `0` is treated
        /// as `1` (sampling nothing would silently skip the file
        /// entirely, never useful).
        #[arg(long)]
        sample: Option<usize>,
        /// Use only the named [[servers]] entry for this run (matched by
        /// its `name` field in the config file), instead of every
        /// configured server. Repeat to pick more than one; they keep
        /// their relative order from the config file. Handy for a quick
        /// `--stat` against one particular provider without editing the
        /// config. Omit to use every configured server, as before this
        /// flag existed.
        #[arg(long = "server")]
        server: Vec<String>,
        /// How much post-processing to do after fetching, mirroring
        /// `sabnzbd`'s per-category processing levels. Each level does
        /// everything the previous one does, plus one more step:
        /// `download` (fetch + assemble only) -> `repair` (+ PAR2
        /// verify/repair) -> `unpack` (+ extract archives) -> `delete`
        /// (+ delete the compressed volumes and PAR2 recovery data once
        /// extraction succeeds, leaving only the release's other files).
        /// Defaults to the config file's `mode`, or `unpack` if that's
        /// unset too.
        #[arg(long, value_enum)]
        mode: Option<ProcessingMode>,
        /// Suppress the live progress panel; only status/result lines print.
        /// Matches `pesto`'s `-q`/`--quiet` convention — handy for tmux/screen
        /// sessions or when output is redirected to a log file.
        #[arg(long, short)]
        quiet: bool,
    },
    #[command(
        about = "Verify that NZB articles are available without downloading",
        after_help = "EXAMPLES:\n  penne check RELEASE.nzb\n  penne check RELEASE.nzb --method head\n  penne check RELEASE.nzb --method body --fail-fast -q\n  penne check *.nzb --fail-fast --quiet\n\n\
Use --method stat for the cheapest index check, head for header storage, or \
body to verify a full article transfer. --fail-fast returns as soon as an \
article is confirmed missing after failover, so it does not produce a full \
availability percentage.\n\n\
EXIT STATUS:\n  0  All checked articles are present\n  1  At least one article is confirmed missing\n  2  Fatal error (configuration, input, or I/O)\n  3  Inconclusive: an article could not be checked"
    )]
    /// Check article availability across one or more `.nzb` files without
    /// downloading. Exits 0 if all articles are present, 1 if any are
    /// confirmed missing (a server returned a definitive "not present"),
    /// 2 on fatal error, 3 if inconclusive (no confirmed-missing article,
    /// but at least one segment never got a real answer from any
    /// configured server — a connection failure, not a `430`).
    Check {
        /// One or more `.nzb` files to check.
        #[arg(required = true)]
        nzb: Vec<PathBuf>,
        /// Which NNTP command to use: `stat` (default, cheapest), `head`
        /// (reads from article storage, catches stale STAT indices), or
        /// `body` (full fetch, discarded — maximum certainty).
        #[arg(long, value_enum, default_value = "stat")]
        method: CheckMethod,
        /// Check only N segments of each file (spread evenly across it)
        /// instead of all.
        #[arg(long)]
        sample: Option<usize>,
        /// STAT commands pipelined per connection (default: 128).
        #[arg(long, default_value = "128")]
        pipeline_depth: usize,
        /// Stop scheduling after an article is confirmed missing on every
        /// applicable failover server. Works with stat, head, and body;
        /// in-flight work finishes cleanly and the remaining articles are
        /// reported as skipped, not missing. Use when only a pass/fail
        /// verdict matters, not a complete availability percentage.
        #[arg(long)]
        fail_fast: bool,
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
        /// Suppress progress bar, print only the final summary.
        #[arg(long, short)]
        quiet: bool,
        /// Use only the named server(s) from the config file.
        #[arg(long = "server")]
        server: Vec<String>,
        /// Check each configured server independently instead of using them
        /// as failover backups. Outputs a separate result for each server.
        #[arg(long)]
        independent_servers: bool,
    },
}
