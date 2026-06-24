//! folder watcher. forwards filesystem changes to the UI as an `fs-changed`
//! event; the UI debounces and calls `scan`. keeping the debounce on the JS side
//! keeps the Rust watcher trivial — it just says "something changed".

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use tauri::{AppHandle, Emitter};

pub fn start(app: AppHandle, folder: &str) -> Result<RecommendedWatcher, String> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(event) => {
                if matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    let _ = app.emit("fs-changed", ());
                }
            }
            // surface watch failures (folder unmounted, OS watch limit, event-queue
            // overflow) instead of letting the watch silently go inert.
            Err(e) => crate::applog::push(&app, "warn", format!("[Video] Folder watch error: {e}")),
        }
    })
    .map_err(|e| e.to_string())?;

    watcher
        .watch(Path::new(folder), RecursiveMode::Recursive)
        .map_err(|e| e.to_string())?;
    Ok(watcher)
}
