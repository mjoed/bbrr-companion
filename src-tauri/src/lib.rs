mod api;
mod applog;
mod auth;
mod keychain;
mod metadata;
mod scan;
mod settings;
mod upload;
mod util;
mod watcher;

use api::{ApiClient, WhoAmI};
use scan::VideoItem;
use settings::Settings;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};

const APP_NAME: &str = "BBRR Companion";

// how often a long-running (tray/autostart) session re-checks for updates. a new
// version found mid-session only surfaces a click-to-update badge — we never
// restart out from under the user. 6h is plenty: the launch check catches most
// users; this only matters for an always-open session.
#[cfg(not(debug_assertions))]
const UPDATE_CHECK_INTERVAL_SECS: u64 = 21600;

#[derive(Default)]
pub(crate) struct AppState {
    pub(crate) watcher: Mutex<Option<notify::RecommendedWatcher>>,
    pub(crate) videos: Mutex<Vec<VideoItem>>,
    pub(crate) control: Arc<AtomicU8>,
    pub(crate) uploading: Arc<AtomicBool>,
    /// set true to abort an in-progress browser sign-in (Cancel button).
    pub(crate) sign_in_cancel: Arc<AtomicBool>,
    /// video ids the user has explicitly queued for upload (one-by-one). the
    /// worker pops from the front; uploads are never started in bulk.
    pub(crate) upload_queue: Mutex<VecDeque<String>>,
    pub(crate) log: Mutex<Vec<applog::LogEntry>>,
    /// epoch-ms of the last "a guild's match failed" warning, to throttle it.
    pub(crate) last_match_warn_ms: Mutex<u128>,
}

/// `.lock()` that recovers from a poisoned mutex instead of re-panicking. the
/// critical sections here are trivial (clone/assign/push/drain) so recovered data
/// is consistent, and one stray panic shouldn't wedge every command locking state.
pub(crate) trait LockSafe<T> {
    fn lock_safe(&self) -> std::sync::MutexGuard<'_, T>;
}
impl<T> LockSafe<T> for Mutex<T> {
    fn lock_safe(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn settings_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("settings.json"))
}

/// persisted known-video cache, so a restart doesn't re-match the whole folder.
fn videos_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("videos.json"))
}

fn persist_videos(app: &tauri::AppHandle, videos: &[VideoItem]) {
    if let Some(path) = videos_path(app) {
        if let Ok(json) = serde_json::to_string(videos) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// load the persisted video cache at startup. stale in-flight uploads (the app
/// was killed mid-upload) are normalized back to "matched" so they re-evaluate
/// and resume rather than appearing stuck "uploading".
fn load_videos(app: &tauri::AppHandle) -> Vec<VideoItem> {
    let Some(path) = videos_path(app) else { return Vec::new() };
    let Ok(content) = std::fs::read_to_string(&path) else { return Vec::new() };
    let mut videos: Vec<VideoItem> = serde_json::from_str(&content).unwrap_or_default();
    for v in &mut videos {
        if v.status == "uploading" {
            v.status = if v.matched.is_some() { "matched".into() } else { "pending".into() };
            v.uploaded_bytes = None;
        }
    }
    videos
}

// ── auth ──────────────────────────────────────────────────────

#[tauri::command]
fn get_settings(app: tauri::AppHandle) -> Result<Settings, String> {
    Ok(Settings::load(&settings_path(&app)?))
}

#[tauri::command(async)]
fn sign_in(state: tauri::State<'_, AppState>) -> Result<WhoAmI, String> {
    let cancel = state.sign_in_cancel.clone();
    cancel.store(false, Ordering::Relaxed);
    let token = auth::sign_in(settings::base_url(), APP_NAME, &cancel)?;
    keychain::store_token(&token)?;
    ApiClient::production(token).whoami()
}

/// abort an in-progress sign-in (the loopback listener stops within ~100ms).
#[tauri::command]
fn cancel_sign_in(state: tauri::State<AppState>) {
    state.sign_in_cancel.store(true, Ordering::Relaxed);
}

#[tauri::command(async)]
fn current_session() -> Result<Option<WhoAmI>, String> {
    let Some(token) = keychain::get_token() else {
        return Ok(None);
    };
    match ApiClient::production(token).whoami() {
        Ok(who) => Ok(Some(who)),
        // a rejected token = signed out. any other failure (offline right after
        // autostart, server down) is surfaced as Err so the UI can keep the
        // session and retry instead of bouncing a valid login to the sign-in screen.
        Err(e) if e == "unauthorized" => Ok(None),
        Err(e) => Err(e),
    }
}

#[tauri::command]
fn sign_out() -> Result<(), String> {
    keychain::clear_token()
}

/// fully exit the app (bypasses the close-to-tray behavior).
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

// ── watch + match ─────────────────────────────────────────────

#[tauri::command]
fn set_folder(app: tauri::AppHandle, folder: String) -> Result<Settings, String> {
    let path = settings_path(&app)?;
    let mut s = Settings::load(&path);
    s.watch_folder = Some(folder);
    s.save(&path)?;
    Ok(s)
}

#[tauri::command]
fn set_selected_guilds(app: tauri::AppHandle, ids: Option<Vec<String>>) -> Result<Settings, String> {
    let path = settings_path(&app)?;
    let mut s = Settings::load(&path);
    s.selected_guild_ids = ids;
    s.save(&path)?;
    Ok(s)
}

/// the merged video list plus whether the server's match call succeeded, so the
/// UI can trust the implied pull list when baselining which pulls already existed
/// (without re-uploading the backlog).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanResult {
    videos: Vec<VideoItem>,
    match_ok: bool,
}

/// scan the watch folder, match all finished recordings, store + return the list.
#[tauri::command(async)]
fn scan(app: tauri::AppHandle, state: tauri::State<'_, AppState>, verify: bool) -> Result<ScanResult, String> {
    let s = Settings::load(&settings_path(&app)?);
    let folder = s.watch_folder.clone().ok_or("No folder selected")?;
    let token = keychain::get_token().ok_or("Not signed in")?;
    let who = ApiClient::production(token.clone()).whoami()?;
    // normally already-"uploaded" files are skipped (only new / unresolved
    // recordings hit the server) and their status preserved. `verify` (periodic
    // backstop + manual scan) re-checks them so a POV deleted on the web stops
    // showing as "already on the server".
    let prev = state.videos.lock_safe().clone();
    let (fresh, match_ok) = scan::scan_and_match(settings::base_url(), &token, &who.guilds, &s.selected_guild_ids, &folder, &prev, verify)?;
    let merged: Vec<VideoItem> = fresh
        .into_iter()
        .map(|f| match prev.iter().find(|cached| cached.id == f.id) {
            // never disturb an in-flight upload.
            Some(cached) if cached.status == "uploading" => cached.clone(),
            // a previously-uploaded file, re-checked by a verify scan:
            Some(cached) if cached.status == "uploaded" => {
                if f.status == "matched" {
                    // pull still exists but the server no longer has this POV
                    // (deleted on the web) → make it re-uploadable.
                    f
                } else if match_ok && f.status == "unmatched" {
                    // server reached and reports no matching pull → the whole
                    // report was deleted, so this upload is genuinely gone; drop
                    // the stale "uploaded". gated on match_ok so a transient/failed
                    // match (mid-reimport churn) can't un-mark it.
                    f
                } else {
                    // cheap scan (cached entry) or an uncertain result → keep it.
                    cached.clone()
                }
            }
            _ => f,
        })
        .collect();
    // activity log for newly-discovered or status-changed videos. uploads are
    // logged by the worker; pending/skipped are too noisy to log.
    for item in &merged {
        let changed = prev.iter().find(|cached| cached.id == item.id).map(|cached| cached.status != item.status).unwrap_or(true);
        if !changed {
            continue;
        }
        let reason_suffix = |reason: &Option<String>| reason.as_deref().map(|x| format!(" ({x})")).unwrap_or_default();
        match item.status.as_str() {
            "matched" => {
                if let Some(m) = &item.matched {
                    applog::push(&app, "info", format!("Matched {} → {} #{} ({})", item.filename, m.boss_name, m.pull_number, m.guild_name));
                }
            }
            "uploaded" => applog::push(&app, "info", format!("Already on server: {}", item.filename)),
            "unmatched" => applog::push(&app, "info", format!("No match: {}{}", item.filename, reason_suffix(&item.reason))),
            "error" => applog::push(&app, "error", format!("{}{}", item.filename, reason_suffix(&item.reason))),
            _ => {}
        }
    }

    // a guild's match call failing holds back auto-upload baseline seeding for
    // every guild (see autoUpload.ts), so warn (throttled) when it happens rather
    // than letting the stall be silent. detected via the per-video failure reason.
    if !match_ok
        && merged.iter().any(|v| v.reason.as_deref().map(|r| r.starts_with("Match request failed")).unwrap_or(false))
    {
        let now = util::now_ms();
        let mut last = state.last_match_warn_ms.lock_safe();
        if now.saturating_sub(*last) > 10 * 60 * 1000 {
            *last = now;
            drop(last);
            applog::push(&app, "warn", "A guild's match request failed — new recordings may not auto-upload until it recovers (see the videos list).");
        }
    }

    *state.videos.lock_safe() = merged.clone();
    persist_videos(&app, &merged);
    Ok(ScanResult { videos: merged, match_ok })
}

#[tauri::command]
fn list_videos(state: tauri::State<AppState>) -> Vec<VideoItem> {
    state.videos.lock_safe().clone()
}

#[tauri::command]
fn get_log(state: tauri::State<AppState>) -> Vec<applog::LogEntry> {
    state.log.lock_safe().clone()
}

/// the full persisted activity log (whole retention window), for "view old
/// entries" — the default view only shows the last 24h.
#[tauri::command]
fn get_full_log(app: tauri::AppHandle) -> Vec<applog::LogEntry> {
    applog::load_all(&app)
}

// ── server push (SSE) ───────────────────────────────────────────────────────
// a background thread holds an SSE connection to /api/povs/events and forwards
// `pulls-changed` (→ live scan, can auto-upload) and `events-connected` on each
// (re)connect (→ passive catch-up scan). reconnects for the app lifetime; a
// 5-minute per-connection timeout bounds reconnect cadence and catches dead
// links. matching stays server-side — this just nudges an earlier re-scan.
fn start_event_listener(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut backoff = 3u64;
        loop {
            let token = match keychain::get_token() {
                Some(t) => t,
                None => {
                    std::thread::sleep(std::time::Duration::from_secs(10));
                    continue;
                }
            };
            match listen_events_once(&app, settings::base_url(), &token) {
                // a connection ended cleanly (5-min timeout / EOF) — reconnect soon.
                Ok(()) => {
                    backoff = 3;
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
                // token rejected: nothing to retry until the user signs in again, so
                // back off hard instead of hammering the endpoint with a dead token.
                Err(e) if e == "unauthorized" => std::thread::sleep(std::time::Duration::from_secs(60)),
                // transient failure — exponential backoff, capped at 60s.
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_secs(backoff));
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    });
}

fn listen_events_once(app: &tauri::AppHandle, base: &str, token: &str) -> Result<(), String> {
    use std::io::BufRead;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(format!("{base}/api/povs/events"))
        .bearer_auth(token)
        .header("Accept", "text/event-stream")
        .send()
        .map_err(|e| e.to_string())?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("unauthorized".into());
    }
    if !resp.status().is_success() {
        return Err(format!("status {}", resp.status()));
    }
    let _ = app.emit("events-connected", ());
    let reader = std::io::BufReader::new(resp);
    let mut event_type = String::new();
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.is_empty() {
            // end of an SSE frame.
            if event_type == "pulls-changed" {
                let _ = app.emit("pulls-changed", ());
            }
            event_type.clear();
        } else if let Some(rest) = line.strip_prefix("event:") {
            event_type = rest.trim().to_string();
        }
        // `data:` lines and `:` keepalive comments are ignored — the companion
        // re-scans regardless of which guild changed.
    }
    Ok(())
}

#[tauri::command(async)]
fn list_pulls(guild_id: String) -> Result<Vec<api::PullSummary>, String> {
    let token = keychain::get_token().ok_or("Not signed in")?;
    ApiClient::production(token).list_pulls(&guild_id)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManualMatchInput {
    guild_id: String,
    guild_name: String,
    pull_id: String,
    boss_name: String,
    pull_number: i64,
    is_kill: bool,
    date: String,
    start_time: String,
}

/// assign an unmatched video to a pull the user picked; queues it for upload.
#[tauri::command]
fn manual_match(app: tauri::AppHandle, state: tauri::State<AppState>, video_id: String, choice: ManualMatchInput) {
    let item = {
        let mut videos = state.videos.lock_safe();
        match videos.iter_mut().find(|v| v.id == video_id) {
            Some(v) => {
                v.status = "matched".into();
                v.reason = None;
                v.matched = Some(scan::MatchedInfo {
                    guild_id: choice.guild_id,
                    guild_name: choice.guild_name,
                    pull_id: choice.pull_id,
                    boss_name: choice.boss_name,
                    pull_number: choice.pull_number,
                    is_kill: choice.is_kill,
                    date: choice.date,
                    start_time: choice.start_time,
                    is_own_pov: true,
                    can_upload: true,
                    existing_video_id: None,
                });
                Some(v.clone())
            }
            None => None,
        }
    };
    if let Some(item) = item {
        if let Some(m) = &item.matched {
            applog::push(&app, "info", format!("Manually matched {} → {} #{}", item.filename, m.boss_name, m.pull_number));
        }
        let _ = app.emit("video-updated", item);
    }
}

#[tauri::command]
fn start_watching(app: tauri::AppHandle, state: tauri::State<AppState>) -> Result<(), String> {
    let s = Settings::load(&settings_path(&app)?);
    let folder = s.watch_folder.clone().ok_or("No folder selected")?;
    let w = watcher::start(app.clone(), &folder)?;
    *state.watcher.lock_safe() = Some(w);
    Ok(())
}

#[tauri::command]
fn stop_watching(state: tauri::State<AppState>) {
    *state.watcher.lock_safe() = None; // dropping the watcher stops it
}

// ── upload ────────────────────────────────────────────────────

/// queue a single matched video for upload. uploads are strictly one-by-one and
/// user-initiated (no bulk "upload all"): each call appends one id to the queue
/// and ensures the worker is running. the worker pops the queue in order.
#[tauri::command]
fn enqueue_upload(app: tauri::AppHandle, video_id: String) {
    let state = app.state::<AppState>();
    // only a matched, not-yet-uploaded video is eligible.
    let eligible = {
        let videos = state.videos.lock_safe();
        videos.iter().any(|v| {
            v.id == video_id
                && v.matched.as_ref().map(|m| m.existing_video_id.is_none()).unwrap_or(false)
        })
    };
    if !eligible {
        return;
    }
    state.control.store(upload::RUN, Ordering::Relaxed);
    // push the id and claim the worker slot under one lock. run_worker only ever
    // clears `uploading` while holding this same lock, so there's no window where
    // a freshly-queued id is left with no worker running (lost wakeup).
    let already_running = {
        let mut q = state.upload_queue.lock_safe();
        if !q.iter().any(|id| id == &video_id) {
            q.push_back(video_id.clone());
        }
        state.uploading.swap(true, Ordering::Relaxed)
    };
    // optimistically reflect "in the upload pipeline" so the row moves under the
    // progress UI immediately (the worker sets real progress once it picks it).
    upload::update_video(&app, &video_id, |v| {
        v.status = "uploading".into();
        v.uploaded_bytes = Some(0);
        v.reason = None;
    });
    if !already_running {
        let app2 = app.clone();
        std::thread::spawn(move || upload::run_worker(app2));
    }
}

#[tauri::command]
fn pause_uploads(state: tauri::State<AppState>) {
    state.control.store(upload::PAUSE, Ordering::Relaxed);
}

#[tauri::command]
fn resume_uploads(state: tauri::State<AppState>) {
    state.control.store(upload::RUN, Ordering::Relaxed);
}

#[tauri::command]
fn cancel_uploads(app: tauri::AppHandle) {
    let state = app.state::<AppState>();
    state.control.store(upload::CANCEL, Ordering::Relaxed);
    // drop anything still queued.
    state.upload_queue.lock_safe().clear();
    // revert every in-flight ("uploading") video back to matched so each shows an
    // Upload button again. enqueue_upload optimistically marks the whole queue
    // "uploading" at 0%, but the worker only reverts the single video it was
    // actively uploading — without this the rest stay stuck at 0%.
    let uploading_ids: Vec<String> = {
        let videos = state.videos.lock_safe();
        videos.iter().filter(|v| v.status == "uploading").map(|v| v.id.clone()).collect()
    };
    for id in uploading_ids {
        upload::update_video(&app, &id, |v| {
            v.status = "matched".into();
            v.uploaded_bytes = None;
        });
    }
}

/// set the upload speed cap in MB/s (None / 0 = unlimited).
#[tauri::command]
fn set_upload_limit(app: tauri::AppHandle, mbps: Option<f64>) -> Result<Settings, String> {
    let path = settings_path(&app)?;
    let mut s = Settings::load(&path);
    // clamp to a sane floor: a near-zero limit makes the throttle sleep for hours
    // (and can overflow Duration::from_secs_f64). None = unlimited.
    s.upload_limit_mbps = mbps.filter(|m| *m > 0.0).map(|m| m.max(0.05));
    s.save(&path)?;
    Ok(s)
}

/// check for an app update. at launch, when idle, we install and restart in place
/// (seamless — the user just opened the app). otherwise — a long-running session,
/// or an active upload — we only emit `update-available` so the header can offer a
/// click-to-update badge, never pulling the app out from under the user. release-only
/// — dev builds have nothing to update; needs `plugins.updater` (pubkey + endpoints)
/// in tauri.conf.json, without which the check just errors and is logged, never fatal.
#[cfg(not(debug_assertions))]
async fn check_for_update(app: tauri::AppHandle, auto_install: bool) -> tauri_plugin_updater::Result<()> {
    use tauri_plugin_updater::UpdaterExt;
    if let Some(update) = app.updater()?.check().await? {
        let uploading = app.state::<AppState>().uploading.load(Ordering::Relaxed);
        if auto_install && !uploading {
            applog::push(&app, "info", format!("Updating to v{}…", update.version));
            update.download_and_install(|_, _| {}, || {}).await?;
            app.restart();
        }
        // running session (or an upload blocked the in-place install): surface the
        // available version so the UI shows a click-to-update badge instead.
        let _ = app.emit("update-available", update.version);
    }
    Ok(())
}

/// user-clicked "update available" badge: re-check and install now, then restart.
/// reports (and does nothing) in dev builds; refuses while an upload is in flight
/// so a transfer is never interrupted.
#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    if cfg!(debug_assertions) {
        return Err("Updates are only available in release builds.".into());
    }
    if app.state::<AppState>().uploading.load(Ordering::Relaxed) {
        return Err("An upload is in progress — try again once it finishes.".into());
    }
    use tauri_plugin_updater::UpdaterExt;
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    let Some(update) = update else {
        return Err("No update available.".into());
    };
    applog::push(&app, "info", format!("Updating to v{}…", update.version));
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart();
}

fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let show = MenuItem::with_id(app, "show", "Open", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let mut builder = TrayIconBuilder::new()
        .tooltip("BBRR Companion")
        .menu(&menu)
        // left-click opens the window; the menu is shown on right-click only.
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
        })
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::default())
        .setup(|app| {
            build_tray(app.handle())?;
            // seed the in-memory activity log from disk so it survives restarts.
            let persisted = applog::load(app.handle());
            if !persisted.is_empty() {
                *app.state::<AppState>().log.lock_safe() = persisted;
            }
            // seed the known-video cache so the first scan only matches new files.
            let videos = load_videos(app.handle());
            if !videos.is_empty() {
                *app.state::<AppState>().videos.lock_safe() = videos;
            }
            // background SSE listener (pull-import push → live re-scan).
            start_event_listener(app.handle().clone());
            // background updates (release only): install in place at launch when
            // idle, then re-check periodically so a long-running tray session still
            // learns about new versions — those only surface a click-to-update badge.
            #[cfg(not(debug_assertions))]
            {
                let launch_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = check_for_update(launch_handle.clone(), true).await {
                        applog::push(&launch_handle, "warn", format!("Update check failed: {e}"));
                    }
                });
                let periodic_handle = app.handle().clone();
                std::thread::spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_secs(UPDATE_CHECK_INTERVAL_SECS));
                    let h = periodic_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Err(e) = check_for_update(h.clone(), false).await {
                            applog::push(&h, "warn", format!("Update check failed: {e}"));
                        }
                    });
                });
            }
            Ok(())
        })
        // close button hides to the tray instead of quitting (use tray → Quit).
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            sign_in,
            cancel_sign_in,
            current_session,
            sign_out,
            quit_app,
            set_folder,
            set_selected_guilds,
            scan,
            list_videos,
            get_log,
            get_full_log,
            list_pulls,
            manual_match,
            start_watching,
            stop_watching,
            enqueue_upload,
            pause_uploads,
            resume_uploads,
            cancel_uploads,
            set_upload_limit,
            install_update
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
