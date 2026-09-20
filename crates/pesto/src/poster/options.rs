//! Borrowed per-run inputs shared by the public facades and the orchestrator.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::config::Config;
use crate::nntp::pool::ConnectionBroker;
use crate::progress::ProgressSender;
use crate::walk::InputFile;

/// Internal run inputs. The public entry points keep their historical
/// argument lists and only assemble this struct, so the orchestrator has a
/// single, named parameter.
pub(super) struct RunOptions<'a> {
    pub(super) config: &'a Config,
    pub(super) files: &'a [InputFile],
    pub(super) events: Option<ProgressSender>,
    pub(super) resume_state_path: Option<&'a Path>,
    pub(super) external_cancel: Option<Arc<AtomicBool>>,
    pub(super) entry_label: Option<&'a str>,
    pub(super) broker: Option<Arc<ConnectionBroker>>,
    pub(super) external_pause: Option<Arc<AtomicBool>>,
    pub(super) release_prefix_override: Option<&'a str>,
}
