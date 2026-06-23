//! small shared primitives for the "is this file finished being written?" check.
//! the scan and the pre-upload re-check must use identical logic so they never
//! disagree about when a recording is done — hence a single definition here.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// a file counts as finished once its mtime has been stable for at least this
/// long (the recorder has stopped writing it).
pub const STABLE_MS: u128 = 5000;

/// current wall-clock time in epoch milliseconds (0 on the impossible error).
pub fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// a file's last-modified time in epoch milliseconds (0 if it can't be read).
pub fn mtime_ms(path: &Path) -> u128 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
