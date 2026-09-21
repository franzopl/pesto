use crate::events::{ProgressUpdate, UploadPhase};
use pesto::config::ObfuscateMode;
use std::time::Instant;

#[derive(Debug, Default)]
pub struct UploadProgress {
    pub total_segments: u64,
    pub done_segments: u64,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub start_time: Option<Instant>,
    pub last_speed: f64, // MB/s
    #[allow(dead_code)]
    pub active_connections: usize,
    pub is_cancelled: bool,
    /// True while the user has paused the upload (`p` on the Dashboard).
    /// Posting workers suspend at the next segment-batch boundary; PAR2,
    /// compression and the check/repost passes are unaffected — see
    /// `pesto::poster::post_files_inner`'s doc for the same scoping `cancel`
    /// already has.
    pub is_paused: bool,

    /// Ring buffer of recent speeds (MB/s) for sparkline
    pub speed_history: Vec<f64>,

    /// Per-file progress (populated when upload starts)
    pub files: Vec<FileProgress>,

    /// Current pipeline phase
    pub phase: UploadPhase,

    /// PAR2 encoding progress (runs concurrently with NNTP posting)
    pub par2_done_slices: usize,
    pub par2_total_slices: usize,
    /// Whether PAR2 encode + write phases are fully complete.
    pub par2_finished: bool,

    /// Streaming check queue progress (runs concurrently with NNTP posting,
    /// for the lifetime of the upload rather than as its own phase).
    pub check_checked: u64,
    pub check_failed: u64,

    /// Bytes pre-seeded from par2_bytes_hint; consumed as QueueExtended arrives
    /// so total_bytes never jumps backwards.
    pub par2_hint_remaining: u64,
    /// Segments pre-seeded from par2_segments_hint; consumed as QueueExtended
    /// arrives, mirroring par2_hint_remaining for bytes.
    pub par2_segment_hint_remaining: u64,

    /// Compression progress (tracked separately for the three-bar display)
    pub compress_total_bytes: u64,
    pub compress_done_bytes: u64,
    pub compress_finished: bool,
}

/// Progress of a single file during an active upload.
#[derive(Debug, Clone)]
pub struct FileProgress {
    pub name: String,
    pub total_segments: u64,
    pub done_segments: u64,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub status: FileStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileStatus {
    #[default]
    Pending,
    Active,
    Done,
    Failed,
}

/// Human-readable summary of the settings that will be used for the next upload.
#[derive(Debug, Clone, Default)]
pub struct UploadSettingsSummary {
    pub obfuscate: String,
    pub compression: String,
    pub par2: String,
    pub groups: String,
    pub from: String,
    pub article_size: String,
    pub check: String,
}

// ── Canonical display vocabulary ──────────────────────────────────────────────
//
// One source of truth for how settings are *shown*, so the Dashboard summary,
// the upload-config panel and the Config overrides all read the same. These map
// internal values to display labels only — the stored values (enums, bools and
// the compress-format token used by the cycle handlers) are untouched.

/// Display label for an obfuscation mode.
pub fn obf_label(mode: ObfuscateMode) -> &'static str {
    match mode {
        ObfuscateMode::None => "None",
        ObfuscateMode::Full => "Full",
        ObfuscateMode::FullShared => "Full (shared)",
        ObfuscateMode::Light => "Light (shared, matching)",
        ObfuscateMode::Article => "Article (experimental)",
    }
}

/// Display label for an on/off setting.
pub fn on_off(enabled: bool) -> &'static str {
    if enabled {
        "On"
    } else {
        "Off"
    }
}

/// Display label for a compression-format token (`none`/`zip`/`7z`/`rar`).
/// The token stays the logic value used by the cycle handlers; this only
/// controls how it is rendered (`none` → `Off`).
pub fn compress_label(token: &str) -> String {
    match token {
        "none" | "" => "Off".to_string(),
        "zip" => "Zip".to_string(),
        "rar" => "Rar".to_string(),
        other => other.to_string(),
    }
}

/// The marker shown for an unset / empty value, used everywhere.
pub const UNSET: &str = "—";

impl UploadProgress {
    const MAX_HISTORY: usize = 60; // ~1 minute at 1 sample/sec

    pub fn push_speed_sample(&mut self, speed: f64) {
        self.speed_history.push(speed);
        if self.speed_history.len() > Self::MAX_HISTORY {
            self.speed_history.remove(0);
        }
    }

    /// Reset the aggregate gauges for a new queue item. Uploads run one NZB at a
    /// time and each item's progress events restart from zero, while `apply`
    /// only ever grows `done_segments`/`done_bytes` (so a single item's bar
    /// never jumps backwards). Without this reset the previous item's 100% state
    /// swallows the next item's smaller counts and the bar looks frozen. The
    /// per-file rows and the speed-history sparkline are kept; the clock is
    /// restarted so speed/ETA track the current item.
    pub fn reset_for_item(&mut self) {
        self.total_segments = 0;
        self.done_segments = 0;
        self.total_bytes = 0;
        self.done_bytes = 0;
        self.last_speed = 0.0;
        self.start_time = Some(Instant::now());
        self.phase = UploadPhase::default();
        self.par2_done_slices = 0;
        self.par2_total_slices = 0;
        self.par2_finished = false;
        self.check_checked = 0;
        self.check_failed = 0;
        self.par2_hint_remaining = 0;
        self.par2_segment_hint_remaining = 0;
        self.compress_total_bytes = 0;
        self.compress_done_bytes = 0;
        self.compress_finished = false;
    }
}

impl UploadProgress {
    pub fn progress_pct(&self) -> f64 {
        if self.total_segments == 0 {
            return 0.0;
        }
        (self.done_segments as f64 / self.total_segments as f64 * 100.0).min(100.0)
    }

    pub fn eta_seconds(&self) -> Option<u64> {
        if self.last_speed <= 0.0 || self.total_bytes == 0 {
            return None;
        }
        let remaining = self.total_bytes.saturating_sub(self.done_bytes);
        let mb_remaining = remaining as f64 / (1024.0 * 1024.0);
        Some((mb_remaining / self.last_speed) as u64)
    }

    pub fn apply(&mut self, update: &ProgressUpdate) {
        if let Some((seg, bytes)) = update.queue_extended {
            // Absorb the real PAR2 bytes/segments against the pre-seeded
            // hints so neither total jumps (same logic as pesto's terminal
            // renderer) — only the excess over the hint grows the total.
            if bytes <= self.par2_hint_remaining {
                self.par2_hint_remaining -= bytes;
            } else {
                let excess = bytes - self.par2_hint_remaining;
                self.par2_hint_remaining = 0;
                self.total_bytes += excess;
            }
            if seg <= self.par2_segment_hint_remaining {
                self.par2_segment_hint_remaining -= seg;
            } else {
                let excess = seg - self.par2_segment_hint_remaining;
                self.par2_segment_hint_remaining = 0;
                self.total_segments += excess;
            }
            return;
        }
        if update.total_segments > 0 {
            self.total_segments = update.total_segments;
        }
        if update.total_bytes > 0 {
            self.total_bytes = update.total_bytes;
        }
        if update.done_segments > self.done_segments {
            self.done_segments = update.done_segments;
        }
        if update.done_bytes > self.done_bytes {
            self.done_bytes = update.done_bytes;
        }
        if update.current_speed_mbps > 0.0 {
            self.last_speed = update.current_speed_mbps;
            self.push_speed_sample(update.current_speed_mbps);
        }
        if let Some(ref phase) = update.phase {
            // Track compress progress for the three-bar display
            match phase {
                UploadPhase::Compressing {
                    done_bytes,
                    total_bytes,
                } => {
                    if *total_bytes > 0 {
                        self.compress_total_bytes = *total_bytes;
                    }
                    self.compress_done_bytes = *done_bytes;
                }
                _ if self.compress_total_bytes > 0 && !self.compress_finished => {
                    // Phase moved past Compressing → compression is done
                    self.compress_finished = true;
                    self.compress_done_bytes = self.compress_total_bytes;
                }
                _ => {}
            }
            self.phase = phase.clone();
        }
        if let Some((done, total)) = update.par2_slices {
            self.par2_done_slices = done;
            if total > 0 {
                self.par2_total_slices = total;
            }
        }
        if let Some((checked, failed)) = update.check_progress {
            self.check_checked = checked;
            self.check_failed = failed;
        }
        if update.par2_hint_bytes > 0 {
            self.par2_hint_remaining = update.par2_hint_bytes;
        }
        if update.par2_segment_hint > 0 {
            self.par2_segment_hint_remaining = update.par2_segment_hint;
        }
        if update.par2_complete {
            self.par2_finished = true;
            // Ensure slices show as complete even if counts were imprecise.
            if self.par2_total_slices > 0 {
                self.par2_done_slices = self.par2_total_slices;
            }
        }
    }
}
