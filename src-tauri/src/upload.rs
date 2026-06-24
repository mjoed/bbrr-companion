//! multipart upload with a smooth MB/s throttle and pause/cancel.
//!
//! uploads run on a background worker that picks "ready" videos one at a time.
//! each part is streamed through a [`ThrottledReader`] so the speed cap is
//! applied smoothly (not in 64 MB bursts) and so cancellation interrupts mid-part.

use crate::api::{self, ApiClient};
use crate::metadata;
use crate::scan::VideoItem;
use crate::util::{mtime_ms, now_ms, STABLE_MS};
use crate::LockSafe;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

// control values shared via AppState.control.
pub const RUN: u8 = 0;
pub const PAUSE: u8 = 1;
pub const CANCEL: u8 = 2;

/// cap each read so throttling + cancel checks stay responsive.
const READ_CHUNK: usize = 64 * 1024;
/// transient transport blips (reset/closed connection) are common on large
/// home-connection uploads; retry each part a few times before failing the whole
/// video so one hiccup doesn't sink a 12-video batch.
const MAX_PART_ATTEMPTS: u32 = 4;

#[derive(PartialEq)]
enum SleepResult {
    Done,
    Cancelled,
}

/// sleep ~`ms` in short slices so a CANCEL is noticed promptly (returns early);
/// PAUSE holds without consuming the remaining time. used for retry backoff.
fn sleep_with_control(ms: u64, control: &Arc<AtomicU8>) -> SleepResult {
    let mut left = ms;
    while left > 0 {
        match control.load(Ordering::Relaxed) {
            CANCEL => return SleepResult::Cancelled,
            PAUSE => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            _ => {}
        }
        let step = left.min(100);
        std::thread::sleep(Duration::from_millis(step));
        left -= step;
    }
    SleepResult::Done
}

/// a reader that paces itself to `bytes_per_sec`, reports progress, and aborts
/// when the shared control flag is set to CANCEL.
struct ThrottledReader<R> {
    inner: R,
    bytes_per_sec: Option<f64>,
    start: Instant,
    sent: u64,
    control: Arc<AtomicU8>,
    on_progress: Arc<dyn Fn(u64) + Send + Sync>,
}

impl<R: Read> Read for ThrottledReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.control.load(Ordering::Relaxed) == CANCEL {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let want = buf.len().min(READ_CHUNK);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Ok(0);
        }
        self.sent += n as u64;
        (self.on_progress)(n as u64);
        if let Some(rate) = self.bytes_per_sec {
            if rate > 0.0 {
                let expected = self.sent as f64 / rate;
                let elapsed = self.start.elapsed().as_secs_f64();
                if expected > elapsed {
                    std::thread::sleep(Duration::from_secs_f64(expected - elapsed));
                }
            }
        }
        Ok(n)
    }
}

pub enum UploadOutcome {
    Done(String), // PullVideo id
    Cancelled,
    NotReady, // file changed since match — re-check later
}

/// result of uploading one part. `Cancelled` and `Failed` both tell the caller to
/// abort the multipart upload so no orphaned parts linger in the bucket.
enum PartOutcome {
    Done(String),   // etag
    Cancelled,
    Failed(String), // retries exhausted, a non-retryable error, or a local file error
}

/// abort the multipart upload (removes the uploaded parts from the bucket). if
/// the abort request itself fails, warn the user — the parts may linger until
/// the bucket's incomplete-multipart-upload lifecycle rule removes them.
fn abort_and_log(app: &AppHandle, client: &ApiClient, created: &api::CreateUpload, filename: &str) {
    if let Err(e) = client.abort_upload(&created.object_key, &created.upload_id, created.storage_config_id.as_deref()) {
        crate::applog::push(
            app,
            "warn",
            format!(
                "[Video] Couldn't clean up the interrupted upload of {filename}: {e}. \
                 The bucket may keep the partial parts until its lifecycle rule removes them."
            ),
        );
    }
}

/// upload one matched video. emits `video-updated` progress along the way.
#[allow(clippy::too_many_arguments)]
fn upload_one(
    app: &AppHandle,
    base: &str,
    token: &str,
    video: &VideoItem,
    limit_mbps: Option<f64>,
    control: Arc<AtomicU8>,
) -> Result<UploadOutcome, String> {
    let matched = video.matched.as_ref().ok_or("video has no match")?;
    let player_name = video.player_name.clone().ok_or("video has no player name")?;
    let path = Path::new(&video.path);

    // re-verify the file is still finished and unchanged.
    let size = std::fs::metadata(path).map(|m| m.len()).map_err(|e| e.to_string())?;
    if now_ms().saturating_sub(mtime_ms(path)) < STABLE_MS {
        return Ok(UploadOutcome::NotReady);
    }

    // recorder metadata for the complete call (duration + raw json).
    let (typed, raw) = metadata::parse_sidecar(
        &metadata::sidecar_for(path).ok_or("sidecar disappeared")?,
    )?;
    let duration_s = typed.duration_seconds().map(|d| d.round() as u64);

    let client = ApiClient::new(base.to_string(), token.to_string());
    let created = client.create_upload(&matched.pull_id, &player_name, video.player_realm.as_deref(), size, &video.filename, None)?;

    let bytes_per_sec = limit_mbps.map(|m| m * 1_000_000.0);
    let uploaded = Arc::new(AtomicU64::new(0));
    let last_emit = Arc::new(AtomicU64::new(0));
    // high-water mark of bytes already shown to the UI, so a retried part (which
    // resets `uploaded` to its boundary) can't make the progress bar jump back.
    let shown = Arc::new(AtomicU64::new(0));
    let total = size;

    // one pooled, HTTP/1.1 client for all of this video's parts: reuse avoids a
    // TLS handshake per 64 MB part, and HTTP/1.1 sidesteps the mid-stream
    // RST_STREAM that large uploads to Cloudflare/R2 hit over HTTP/2. no overall
    // timeout (a throttled part can take minutes); tcp_keepalive + connect_timeout
    // (plus TCP_USER_TIMEOUT on linux) bound socket liveness so a half-open/stalled
    // connection errors into the retry path instead of wedging the worker. these
    // are ACK/liveness bounds, not throughput limits, so a slow or speed-limited
    // upload is never tripped.
    let put_client = {
        let builder = reqwest::blocking::Client::builder()
            .user_agent(concat!("BBRRCompanion/", env!("CARGO_PKG_VERSION")))
            .http1_only()
            .connect_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(30));
        // TCP_USER_TIMEOUT is a linux-only socket option in reqwest; on other
        // platforms tcp_keepalive + connect_timeout cover socket liveness.
        #[cfg(target_os = "linux")]
        let builder = builder.tcp_user_timeout(Duration::from_secs(120));
        builder.build().map_err(|e| e.to_string())?
    };

    // progress emitter shared by every part of this video (loop-invariant — it
    // feeds the per-video `uploaded`/`shown` counters, not per-part state).
    let progress: Arc<dyn Fn(u64) + Send + Sync> = {
        let app = app.clone();
        let id = video.id.clone();
        let uploaded = uploaded.clone();
        let last_emit = last_emit.clone();
        let shown = shown.clone();
        Arc::new(move |delta: u64| {
            let done = uploaded.fetch_add(delta, Ordering::Relaxed) + delta;
            // clamp the emitted value to the high-water mark so a retry's reset
            // of `uploaded` never shows the bar moving backwards.
            let shown_bytes = done.max(shown.fetch_max(done, Ordering::Relaxed));
            let now = now_ms() as u64;
            if now.saturating_sub(last_emit.load(Ordering::Relaxed)) >= 200 {
                last_emit.store(now, Ordering::Relaxed);
                let _ = app.emit("upload-progress", serde_json::json!({ "id": id, "uploadedBytes": shown_bytes }));
            }
        })
    };

    if created.part_size == 0 {
        abort_and_log(app, &client, &created, &video.filename);
        return Err("server returned an invalid part size".into());
    }
    let mut parts: Vec<(u32, String)> = Vec::with_capacity(created.part_urls.len());
    for (i, url) in created.part_urls.iter().enumerate() {
        let offset = (i as u64) * created.part_size;
        // guard against a server part list longer than the file needs: an
        // unchecked `total - offset` would underflow (panic in debug, wrap in
        // release) and produce a malformed PUT.
        let Some(remaining) = total.checked_sub(offset).filter(|r| *r > 0) else {
            abort_and_log(app, &client, &created, &video.filename);
            return Err("server returned more upload parts than the file needs".into());
        };
        let part_len = created.part_size.min(remaining);
        match upload_one_part(
            &put_client, url, path, offset, part_len, bytes_per_sec,
            &control, &uploaded, &progress, i, &video.filename, app,
        ) {
            PartOutcome::Done(etag) => parts.push(((i + 1) as u32, etag)),
            PartOutcome::Cancelled => {
                abort_and_log(app, &client, &created, &video.filename);
                return Ok(UploadOutcome::Cancelled);
            }
            PartOutcome::Failed(msg) => {
                abort_and_log(app, &client, &created, &video.filename);
                return Err(msg);
            }
        }
    }

    let video_id = client.complete_upload(
        &matched.pull_id,
        &player_name,
        &created.object_key,
        &created.upload_id,
        &parts,
        total,
        duration_s,
        &raw,
        created.storage_config_id.as_deref(),
    )?;

    Ok(UploadOutcome::Done(video_id))
}

/// upload a single part, retrying transient failures with backoff. returns
/// `Done(etag)` on success, `Cancelled` if a cancel is observed (before, during,
/// or between retries), or `Failed(msg)` once retries are exhausted, a
/// non-retryable error hits, or the local file can't be opened/seeked (the caller
/// aborts the multipart upload on `Failed`, so no orphaned parts are left behind).
#[allow(clippy::too_many_arguments)]
fn upload_one_part(
    put_client: &reqwest::blocking::Client,
    url: &str,
    path: &Path,
    offset: u64,
    part_len: u64,
    bytes_per_sec: Option<f64>,
    control: &Arc<AtomicU8>,
    uploaded: &Arc<AtomicU64>,
    progress: &Arc<dyn Fn(u64) + Send + Sync>,
    part_index: usize,
    filename: &str,
    app: &AppHandle,
) -> PartOutcome {
    // pause (honouring cancel) before starting the part — mid-part stalls can
    // trip S3 timeouts, so hold between parts instead.
    loop {
        match control.load(Ordering::Relaxed) {
            CANCEL => return PartOutcome::Cancelled,
            PAUSE => std::thread::sleep(Duration::from_millis(250)),
            _ => break,
        }
    }

    let mut attempt: u32 = 0;
    loop {
        attempt += 1;

        // the reader is consumed by the request body, so each attempt rebuilds a
        // fresh file handle at the part offset. reset the progress counter to this
        // part's boundary so a failed attempt's bytes aren't double-counted.
        uploaded.store(offset, Ordering::Relaxed);
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(e) => return PartOutcome::Failed(format!("can't open recording: {e}")),
        };
        if let Err(e) = f.seek(SeekFrom::Start(offset)) {
            return PartOutcome::Failed(format!("can't read recording: {e}"));
        }
        let reader = ThrottledReader {
            inner: f.take(part_len),
            bytes_per_sec,
            start: Instant::now(),
            sent: 0,
            control: control.clone(),
            on_progress: progress.clone(),
        };
        let body = reqwest::blocking::Body::sized(reader, part_len);

        match api::put_part(put_client, url, body) {
            Ok(etag) => return PartOutcome::Done(etag),
            Err(e) => {
                // a cancel mid-PUT surfaces as a transport error — honour it.
                if control.load(Ordering::Relaxed) == CANCEL {
                    return PartOutcome::Cancelled;
                }
                if !e.retryable || attempt >= MAX_PART_ATTEMPTS {
                    return PartOutcome::Failed(format!("{} (after {attempt} attempts)", e.msg));
                }
                crate::applog::push(
                    app,
                    "warn",
                    format!(
                        "[Video] Part {} of {} failed ({}), retrying ({attempt}/{MAX_PART_ATTEMPTS})…",
                        part_index + 1,
                        filename,
                        e.msg,
                    ),
                );
                // exponential backoff (0.5s, 1s, 2s), responsive to cancel.
                let backoff_ms = 500u64 * (1u64 << (attempt - 1));
                if sleep_with_control(backoff_ms, control) == SleepResult::Cancelled {
                    return PartOutcome::Cancelled;
                }
            }
        }
    }
}

/// update a video in shared state and emit `video-updated`.
pub fn update_video(app: &AppHandle, id: &str, f: impl FnOnce(&mut VideoItem)) {
    let state = app.state::<crate::AppState>();
    let item = {
        let mut videos = state.videos.lock_safe();
        match videos.iter_mut().find(|v| v.id == id) {
            Some(v) => {
                f(v);
                v.clone()
            }
            None => return,
        }
    };
    let _ = app.emit("video-updated", item);
}

/// background worker: uploads the user-queued videos one at a time until the
/// queue is empty or cancelled. `uploading` guards against running two workers.
pub fn run_worker(app: AppHandle) {
    let state = app.state::<crate::AppState>();
    let settings_path = app
        .path()
        .app_config_dir()
        .map(|d| d.join("settings.json"))
        .unwrap_or_default();
    let base_url = crate::settings::base_url();
    let token = match crate::keychain::get_token() {
        Some(t) => t,
        None => {
            // clear the flag under the queue lock (see the pop loop below).
            let _q = state.upload_queue.lock_safe();
            state.uploading.store(false, Ordering::Relaxed);
            return;
        }
    };
    let control = state.control.clone();

    loop {
        // pop the next id, or stop. `uploading` is cleared here while the queue
        // lock is held, so it's atomic against enqueue_upload's push+swap — a
        // freshly-queued id can never be stranded with no worker (lost wakeup).
        let id = {
            let mut q = state.upload_queue.lock_safe();
            if control.load(Ordering::Relaxed) == CANCEL {
                q.clear();
                state.uploading.store(false, Ordering::Relaxed);
                break;
            }
            match q.pop_front() {
                Some(id) => id,
                None => {
                    state.uploading.store(false, Ordering::Relaxed);
                    break;
                }
            }
        };
        let next = {
            let videos = state.videos.lock_safe();
            videos.iter().find(|v| v.id == id).cloned()
        };
        // skip anything that vanished or is already on the server.
        let Some(video) = next else { continue };
        if video.matched.as_ref().map(|m| m.existing_video_id.is_some()).unwrap_or(true) {
            continue;
        }

        update_video(&app, &video.id, |v| {
            v.status = "uploading".into();
            v.uploaded_bytes = Some(0);
            v.reason = None;
        });
        crate::applog::push(&app, "info", format!("[Video] Uploading {}…", video.filename));

        // re-read the limit each video so a change applies promptly.
        let limit = crate::settings::Settings::load(&settings_path).upload_limit_mbps;
        match upload_one(&app, base_url, &token, &video, limit, control.clone()) {
            Ok(UploadOutcome::Done(video_id)) => {
                let guild = video.matched.as_ref().map(|m| m.guild_name.clone()).unwrap_or_default();
                update_video(&app, &video.id, |v| {
                    v.status = "uploaded".into();
                    v.reason = Some("Uploaded".into());
                    if let Some(m) = v.matched.as_mut() {
                        m.existing_video_id = Some(video_id);
                    }
                });
                crate::applog::push(&app, "info", format!("[Video] Uploaded {} to {}", video.filename, guild));
            }
            Ok(UploadOutcome::Cancelled) => {
                {
                    let mut q = state.upload_queue.lock_safe();
                    q.clear();
                    state.uploading.store(false, Ordering::Relaxed);
                }
                update_video(&app, &video.id, |v| {
                    v.status = "matched".into();
                    v.uploaded_bytes = None;
                });
                crate::applog::push(&app, "info", "[Video] Uploads cancelled");
                break;
            }
            Ok(UploadOutcome::NotReady) => {
                // the file changed / isn't settled since match — requeue so a
                // manual upload click isn't lost, then pause briefly (cancel-aware)
                // so we don't hot-loop while it finishes writing.
                update_video(&app, &video.id, |v| {
                    v.status = "pending".into();
                    v.reason = Some("Finishing up…".into());
                });
                state.upload_queue.lock_safe().push_back(video.id.clone());
                sleep_with_control(1500, &control);
            }
            Err(e) => {
                crate::applog::push(&app, "error", format!("[Video] Upload failed for {}: {}", video.filename, e));
                update_video(&app, &video.id, |v| {
                    v.status = "error".into();
                    v.uploaded_bytes = None;
                    v.reason = Some(e);
                });
            }
        }
    }

    let _ = app.emit("uploads-idle", ());
}
