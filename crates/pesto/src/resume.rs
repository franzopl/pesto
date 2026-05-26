//! Upload resume: persist already-posted segments so an interrupted run can
//! continue without re-sending articles the server already accepted.
//!
//! State is stored as a JSON file (`.pesto-state`) beside the `.nzb` output.
//! Each record maps a `(relative_file_name, part_number)` pair to the
//! `Message-ID` that was issued when the segment was originally posted.
//! On resume, workers skip segments present in the state and inject the stored
//! `Message-ID` directly into the results, so the final `.nzb` is correct.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Persistent state for a single upload session.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ResumeState {
    /// Key: `"{file_name}\0{part}"`. Value: the posted `Message-ID`.
    segments: HashMap<String, String>,
}

impl ResumeState {
    fn key(file_name: &str, part: u32) -> String {
        format!("{file_name}\0{part}")
    }

    /// Load state from `path`. Returns an empty state when the file does not
    /// exist (fresh run) or cannot be parsed (corrupt state file).
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading resume state `{}`", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parsing resume state `{}`", path.display()))
    }

    /// Write the current state to `path`, creating or truncating the file.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string(self).context("serialising resume state")?;
        std::fs::write(path, text)
            .with_context(|| format!("writing resume state `{}`", path.display()))
    }

    /// Return the stored `Message-ID` for a segment, if it was already posted.
    pub fn get(&self, file_name: &str, part: u32) -> Option<&str> {
        self.segments
            .get(&Self::key(file_name, part))
            .map(String::as_str)
    }

    /// Record a successfully posted segment.
    pub fn record(&mut self, file_name: &str, part: u32, message_id: &str) {
        self.segments
            .insert(Self::key(file_name, part), message_id.to_string());
    }

    /// Number of segments recorded in this state.
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut s = ResumeState::default();
        s.record("file.bin", 1, "abc@example.com");
        s.record("file.bin", 2, "def@example.com");
        assert_eq!(s.get("file.bin", 1), Some("abc@example.com"));
        assert_eq!(s.get("file.bin", 3), None);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = ResumeState::default();
        s.record("a.bin", 1, "id1@x");
        s.save(&path).unwrap();
        let loaded = ResumeState::load(&path).unwrap();
        assert_eq!(loaded.get("a.bin", 1), Some("id1@x"));
    }

    #[test]
    fn missing_file_returns_empty() {
        let state = ResumeState::load(Path::new("/nonexistent/path.json")).unwrap();
        assert!(state.is_empty());
    }
}
