//! Archive addon configuration
//!
//! Archive addon manages its own configuration independently.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArchiveConfig {
    pub enabled: bool,
    pub archive_time: String,
    pub timezone: Option<String>,
    pub retention_days: u32,
    /// Archive records older than this many days. If 0, archives records from the previous day.
    #[serde(default)]
    pub archive_after_days: u32,
    #[serde(default)]
    pub archive_dir: Option<String>,
}

impl ArchiveConfig {
    /// Returns the effective archive directory, deriving from recording path if not set.
    pub fn effective_archive_dir(&self, recording_path: &str) -> String {
        self.archive_dir
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| format!("{}/archive", recording_path.trim_end_matches('/')))
    }
}
