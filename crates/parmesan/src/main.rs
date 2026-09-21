mod cli;
mod command;
mod memory;

use anyhow::Result;

fn main() -> Result<()> {
    // Must run before any thread exists — see `memory` module docs (#137).
    memory::tune_allocator();
    tracing_subscriber::fmt::init();

    let command = cli::parse_env();
    // A hand-built runtime avoids `#[tokio::main]` sizing worker threads to
    // `nproc`; see `memory::build_runtime`.
    memory::build_runtime()?.block_on(command::run(command))
}
