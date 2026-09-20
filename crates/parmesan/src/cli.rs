use clap::{Args, Parser, Subcommand};
use parmesan::{EncoderLayout, SimdPath};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "parmesan",
    version = parmesan::DISPLAY_VERSION,
    about = "Fast, standalone PAR2 creation tool"
)]
struct Cli {
    #[command(subcommand)]
    pub(super) command: Command,
}

#[derive(Subcommand, Debug)]
pub(super) enum Command {
    /// Create a new PAR2 recovery set for the given files (default action).
    Create(CreateArgs),
    /// Verify files against an existing PAR2 recovery set.
    Verify(VerifyArgs),
    /// Repair damaged or missing files using an existing PAR2 recovery set.
    Repair(RepairArgs),
}

#[derive(Args, Debug)]
pub(super) struct CreateArgs {
    /// Files or directories to protect.
    #[arg(required = true)]
    pub(super) files: Vec<PathBuf>,

    /// Percentage of recovery data to generate.
    #[arg(short, long, default_value_t = 10)]
    pub(super) recovery_pct: u8,

    /// Manual PAR2 slice size, e.g. "1 MiB".
    #[arg(short = 's', long)]
    pub(super) slice_size: Option<String>,

    /// Target number of input slices.
    #[arg(short = 'n', long)]
    pub(super) slice_count: Option<usize>,

    /// Exact number of recovery blocks to generate.
    #[arg(long)]
    pub(super) recovery_count: Option<usize>,

    /// Maximum RAM for recovery buffers, e.g. "1 GiB".
    #[arg(short = 'm', long, default_value = "1 GiB")]
    pub(super) memory_limit: String,

    /// Number of threads for parallel compute.
    #[arg(short = 't', long)]
    pub(super) threads: Option<usize>,

    /// Force a specific SIMD multiplication backend.
    #[arg(long, value_enum, default_value_t = SimdPath::Auto)]
    pub(super) simd: SimdPath,

    /// Recovery-buffer layout. `smart` is the production auto path.
    /// `affine512` is the packed AVX-512+GFNI kernel (not auto on SPR yet).
    #[arg(long, value_enum, default_value_t = EncoderLayout::Smart)]
    pub(super) encoder: EncoderLayout,

    /// Output directory for PAR2 files.
    #[arg(short, long)]
    pub(super) out_dir: Option<PathBuf>,

    /// Base name for output PAR2 files (default: first input file's name).
    #[arg(short = 'b', long)]
    pub(super) base_name: Option<String>,

    /// Suppress all progress and geometry output.
    #[arg(short = 'q', long)]
    pub(super) quiet: bool,

    /// Overwrite existing PAR2 files instead of failing.
    #[arg(short = 'O', long)]
    pub(super) overwrite: bool,

    /// Skip generating the index (.par2) file.
    #[arg(long)]
    pub(super) no_index: bool,

    /// Recurse into directories.
    #[arg(short = 'R', long)]
    pub(super) recurse: bool,

    /// Embed a comment in the PAR2 Creator packet (repeatable).
    #[arg(short = 'c', long)]
    pub(super) comment: Vec<String>,

    /// Exponent of the first recovery block (default 0).
    #[arg(short = 'e', long, default_value_t = 0)]
    pub(super) recovery_offset: usize,
}

#[derive(Args, Debug)]
pub(super) struct VerifyArgs {
    /// Path to the PAR2 index file (e.g. "movie.mkv.par2").
    pub(super) index: PathBuf,

    /// Suppress the per-file report; only the summary line is printed.
    #[arg(short = 'q', long)]
    pub(super) quiet: bool,

    /// Emit a machine-readable JSON report instead of human-readable text.
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Args, Debug)]
pub(super) struct RepairArgs {
    /// Path to the PAR2 index file (e.g. "movie.mkv.par2").
    pub(super) index: PathBuf,

    /// Report what would be repaired without writing any files.
    #[arg(long)]
    pub(super) dry_run: bool,

    /// Write repaired files under this directory instead of overwriting
    /// damaged/missing originals in place.
    #[arg(short, long)]
    pub(super) out_dir: Option<PathBuf>,

    /// Suppress the per-file report; only the summary line is printed.
    #[arg(short = 'q', long)]
    pub(super) quiet: bool,

    /// Emit a machine-readable JSON report instead of human-readable text.
    #[arg(long)]
    pub(super) json: bool,
}

const KNOWN_FIRST_ARGS: [&str; 8] = [
    "create",
    "verify",
    "repair",
    "help",
    "-h",
    "--help",
    "-V",
    "--version",
];

pub(super) fn parse_env() -> Command {
    let mut args: Vec<String> = std::env::args().collect();
    if let Some(first) = args.get(1) {
        if !KNOWN_FIRST_ARGS.contains(&first.as_str()) {
            args.insert(1, "create".to_string());
        }
    }
    Cli::parse_from(args).command
}
