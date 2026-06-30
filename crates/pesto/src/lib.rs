//! Public API of `pesto`, intended for integration with `upapasta`.
//!
//! `pesto` is a fast, lean Usenet poster: it yEnc-encodes files, posts the
//! resulting articles over NNTP and emits an `.nzb` file. See ROADMAP.md for
//! the development plan.
//!
//! # Embedding
//!
//! The minimal surface for embedding callers is [`post`]: pass a resolved
//! [`config::Config`] and the list of [`poster::InputFile`]s; get back a
//! [`poster::PostOutcome`]. Progress events are delivered on the
//! [`progress::ProgressReceiver`] returned alongside the outcome; drop it to
//! silence reporting.
//!
//! ```ignore
//! # async fn example() -> anyhow::Result<()> {
//! use pesto::{config::Config, walk::InputFile};
//!
//! let config = Config { /* ... */ };
//! let files = vec![InputFile { path: "movie.mkv".into(), real_name: None }];
//! let (outcome, _events) = pesto::post(config, files).await?;
//! println!("posted {} segments", outcome.segments.len());
//! # Ok(())
//! # }
//! ```

pub mod article;
pub mod cancel;
pub mod compress;
pub mod config;
pub mod history;
pub mod hooks;
pub mod logging;
pub mod nfo;
pub mod nntp;
pub mod notify;
pub mod nzb;
pub use parmesan as par2;
pub mod poster;
pub mod progress;
pub mod resume;
pub mod ui;
pub mod upload;
pub mod walk;
pub mod yenc;

/// Post `files` to Usenet using `config` and return the outcome together with
/// a [`progress::ProgressReceiver`] the caller can drain for live updates.
///
/// Dropping the receiver is safe: the poster continues unimpeded.
pub async fn post(
    config: config::Config,
    files: Vec<walk::InputFile>,
) -> anyhow::Result<(poster::PostOutcome, progress::ProgressReceiver)> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    cancel::spawn_listener(flag.clone());
    let outcome = poster::post_files_with_progress_and_cancel(
        &config,
        &files,
        Some(tx),
        None,
        Some(flag),
        None,
    )
    .await?;
    Ok((outcome, rx))
}

/// Like [`post`] but accepts an external cancel flag.
///
/// Set `cancel` to `true` at any point to abort the upload at the next segment
/// boundary. The outcome returned reflects the partial run (cancelled flag is
/// available on [`poster::PostOutcome`]).
pub async fn post_cancelable(
    config: config::Config,
    files: Vec<walk::InputFile>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<(poster::PostOutcome, progress::ProgressReceiver)> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = poster::post_files_with_progress_and_cancel(
        &config,
        &files,
        Some(tx),
        None,
        Some(cancel),
        None,
    )
    .await?;
    Ok((outcome, rx))
}
