//! best-effort auto-detection of the WoW Logs folder and the recording folder for
//! first-run setup, so the app works without manual path-picking.
//!
//! the most reliable source is WarcraftRecorder's own config (config-v3.json in the
//! OS roaming config dir): it stores both the video `storagePath` and the WoW
//! `retailLogPath`, regardless of where WoW or the videos actually live. when WR
//! isn't present we fall back to scanning the common WoW install locations for the
//! Logs folder. Archon keeps its output folder in a Chromium store we can't cleanly
//! read, so Archon-only users pick the folder manually.

use std::path::{Path, PathBuf};

/// pull a non-empty string field out of WarcraftRecorder's config-v3.json.
/// `config_dir` is the OS roaming config dir (%APPDATA% on Windows, ~/Library/
/// Application Support on macOS) — the parent of WR's own config folder.
fn wr_config_value(config_dir: &Path, key: &str) -> Option<String> {
    let path = config_dir.join("WarcraftRecorder").join("config-v3.json");
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let s = json.get(key)?.as_str()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// the WoW retail Logs directory: WarcraftRecorder's configured `retailLogPath`
/// first (works for any install location), else a scan of the usual install spots.
pub fn wow_logs_folder(config_dir: Option<&Path>) -> Option<String> {
    config_dir
        .and_then(|d| wr_config_value(d, "retailLogPath"))
        .filter(|p| Path::new(p).is_dir())
        .or_else(scan_wow_logs_folder)
}

/// the recording/video folder to watch: WarcraftRecorder's `storagePath`. (Archon
/// stores its folder in a Chromium store we don't parse — those users pick it.)
pub fn recording_folder(config_dir: Option<&Path>) -> Option<String> {
    config_dir
        .and_then(|d| wr_config_value(d, "storagePath"))
        .filter(|p| Path::new(p).is_dir())
}

/// scan the common Battle.net install locations for `...\_retail_\Logs`.
fn scan_wow_logs_folder() -> Option<String> {
    for base in wow_install_candidates() {
        let logs = base.join("_retail_").join("Logs");
        if logs.is_dir() {
            return Some(logs.to_string_lossy().to_string());
        }
    }
    None
}

#[cfg(windows)]
fn wow_install_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // the usual Battle.net install layouts, across the drive letters people
    // actually use.
    for drive in ["C", "D", "E", "F", "G"] {
        for root in [
            format!("{drive}:\\Program Files (x86)\\World of Warcraft"),
            format!("{drive}:\\Program Files\\World of Warcraft"),
            format!("{drive}:\\World of Warcraft"),
            format!("{drive}:\\Games\\World of Warcraft"),
        ] {
            out.push(PathBuf::from(root));
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn wow_install_candidates() -> Vec<PathBuf> {
    vec![PathBuf::from("/Applications/World of Warcraft")]
}

#[cfg(not(any(windows, target_os = "macos")))]
fn wow_install_candidates() -> Vec<PathBuf> {
    Vec::new()
}
