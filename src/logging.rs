//! Logging initialisation for the `--verbose` flag.
//!
//! Levels:
//!   `-v`   → INFO  — worker state, file discovery, PAR2 geometry
//!   `-vv`  → DEBUG — NNTP commands and responses (credentials masked)
//!   `-vvv` → TRACE — fine-grained timing, buffer events
//!
//! The subscriber writes to stderr so it does not interfere with the JSON
//! output mode on stdout. When a `log_file` path is provided the output goes
//! to that file instead and the terminal panel is not suppressed.

use std::path::Path;

use anyhow::Result;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// Initialise the global tracing subscriber.
///
/// `verbose` is the number of `-v` flags supplied (0 = no logging, 1 = INFO,
/// 2 = DEBUG, 3+ = TRACE).  `log_file` redirects output to a file; when
/// `None` the logs go to stderr.
///
/// Calling this more than once has no effect (the global subscriber can only
/// be set once).
pub fn init(verbose: u8, log_file: Option<&Path>) -> Result<()> {
    if verbose == 0 {
        return Ok(());
    }

    let level = match verbose {
        1 => LevelFilter::INFO,
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    };

    // RUST_LOG overrides the -v level so power users can fine-tune per module.
    let filter = EnvFilter::builder()
        .with_default_directive(level.into())
        .from_env_lossy();

    if let Some(path) = log_file {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| anyhow::anyhow!("opening log file `{}`: {e}", path.display()))?;
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .with_filter(filter),
        );
        tracing::subscriber::set_global_default(subscriber).ok();
    } else {
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(filter),
        );
        tracing::subscriber::set_global_default(subscriber).ok();
    }

    Ok(())
}

/// Return `true` when the active log level is DEBUG or finer, which means
/// NNTP command traces are being emitted. The caller can use this to suppress
/// the terminal panel renderer (rendering and trace output share stderr and
/// would corrupt each other).
pub fn debug_enabled() -> bool {
    tracing::enabled!(tracing::Level::DEBUG)
}

/// Emit a structured INFO event with OS and CPU capability information.
///
/// Called once at startup when `-v` is active. Useful for bug reports: the
/// log captures exactly which SIMD paths are available on the reporter's CPU.
pub fn log_system_info() {
    if !tracing::enabled!(tracing::Level::INFO) {
        return;
    }

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    // CPU feature detection (x86_64)
    #[cfg(target_arch = "x86_64")]
    let cpu_features = {
        let mut feats = Vec::new();
        if std::is_x86_feature_detected!("avx512f")
            && std::is_x86_feature_detected!("avx512bw")
            && std::is_x86_feature_detected!("gfni")
        {
            feats.push("avx512+gfni");
        }
        if std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("gfni") {
            feats.push("avx2+gfni");
        } else if std::is_x86_feature_detected!("avx2") {
            feats.push("avx2");
        }
        if std::is_x86_feature_detected!("ssse3") {
            feats.push("ssse3");
        }
        feats.join(",")
    };

    #[cfg(target_arch = "aarch64")]
    let cpu_features = "neon".to_string();

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let cpu_features = "scalar".to_string();

    tracing::info!(os, arch, cpu_features = %cpu_features, "system info");
}
