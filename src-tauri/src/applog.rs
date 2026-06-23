//! activity log. each entry is pushed to capped in-memory state and appended to
//! a JSONL file on disk, so the log is a durable record across restarts, not just
//! the current session. each push also emits a `log` event so the UI appends it
//! live.

use crate::LockSafe;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

/// in-memory window (also seeded from disk on launch).
const CAP: usize = 1000;
/// max lines retained on disk (a safety bound on top of the age cutoff).
const FILE_CAP: usize = 10_000;
/// entries older than this are dropped from disk on startup.
const RETENTION_MS: f64 = 14.0 * 24.0 * 60.0 * 60.0 * 1000.0;

/// serializes the disk append across threads (scan, the upload worker, and
/// command handlers all push concurrently) so JSONL lines can't interleave —
/// O_APPEND write atomicity isn't guaranteed on all platforms (notably Windows).
static FILE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub ts: f64,
    pub level: String, // info | warn | error
    pub message: String,
}

fn log_file(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("activity.jsonl"))
}

pub fn push(app: &AppHandle, level: &str, message: impl Into<String>) {
    let entry = LogEntry {
        ts: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0),
        level: level.to_string(),
        message: message.into(),
    };

    let state = app.state::<crate::AppState>();
    {
        let mut log = state.log.lock_safe();
        log.push(entry.clone());
        let len = log.len();
        if len > CAP {
            log.drain(0..len - CAP);
        }
    }

    // append to disk (best-effort — a logging failure must never break the app).
    if let Some(path) = log_file(app) {
        if let Ok(line) = serde_json::to_string(&entry) {
            let _guard = FILE_LOCK.lock_safe();
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    let _ = app.emit("log", entry);
}

/// read the persisted log, dropping entries older than the retention window and
/// capping the on-disk file to `FILE_CAP` (rewriting if anything was pruned).
fn read_pruned(app: &AppHandle) -> Vec<LogEntry> {
    let Some(path) = log_file(app) else { return Vec::new() };
    let Ok(content) = std::fs::read_to_string(&path) else { return Vec::new() };
    let mut entries: Vec<LogEntry> = content
        .lines()
        .filter_map(|l| serde_json::from_str::<LogEntry>(l).ok())
        .collect();
    let before = entries.len();

    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0);
    let cutoff = now - RETENTION_MS;
    entries.retain(|e| e.ts >= cutoff);
    if entries.len() > FILE_CAP {
        entries = entries.split_off(entries.len() - FILE_CAP);
    }

    if entries.len() != before {
        let rewritten: String = entries
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(&path, if rewritten.is_empty() { String::new() } else { format!("{rewritten}\n") });
    }
    entries
}

/// most recent `CAP` entries — seeds the in-memory state at startup.
pub fn load(app: &AppHandle) -> Vec<LogEntry> {
    let entries = read_pruned(app);
    let start = entries.len().saturating_sub(CAP);
    entries[start..].to_vec()
}

/// the full retained log (for the UI's "view old entries").
pub fn load_all(app: &AppHandle) -> Vec<LogEntry> {
    read_pruned(app)
}
