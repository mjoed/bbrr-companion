//! combat-log tailer for livelog acceleration. while a guild is actively
//! livelog-ing a WCL report, this tails the local WoWCombatLog.txt; when a
//! tracked raid encounter ends it pings the server so it can poll WCL right away
//! instead of waiting up to 60s for its own timer. the server's 60s timer stays
//! as a safety net, so a missed/late signal only costs latency, never a pull.
//!
//! the tailer runs on a background thread (mirroring the upload worker) and is
//! only alive when ALL of: livelog watching enabled, at least one active-livelog
//! guild is known, and a logs folder is set. it polls the file (no notify watch)
//! and always seeks to end on (re)open — history is never parsed.

use crate::api::{ApiClient, EncounterSignal};
use crate::LockSafe;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

/// every combat-log activity-log line carries this prefix (the video pipeline
/// uses "[Video] "), so both show distinctly in the UI activity list.
const TAG: &str = "[Combatlog] ";

/// how often the tailer wakes to read appended bytes.
const POLL_MS: u64 = 500;

/// how often we beat the server while actively tailing the log (server TTL ~50s).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);

/// shared controller for the tailer thread. `tailer` holds the live tailer's OWN
/// stop flag — Some = a tailer is running (or was just asked to stop), None =
/// none running — always read+written under its mutex so concurrent reconciles
/// can't double-spawn or strand a tailer. giving each tailer its own flag means a
/// fresh tailer started while a previous one is still winding down never shares a
/// flag with it (this is what fixes the stop->start flap race).
#[derive(Default)]
pub struct CombatLogState {
    /// guild ids that currently have an active livelog session (from SSE). the
    /// tailer signals every guild in this set on a qualifying ENCOUNTER_END.
    pub active_guilds: Mutex<Vec<String>>,
    /// the running tailer's private stop flag: set true to ask it to exit (it does
    /// within ~POLL_MS). None when no tailer is running.
    tailer: Mutex<Option<Arc<AtomicBool>>>,
}

impl CombatLogState {
    /// snapshot of the active-livelog guild set.
    pub fn active_guilds(&self) -> Vec<String> {
        self.active_guilds.lock_safe().clone()
    }
}

/// (re)evaluate whether the tailer should be running and start/stop it to match.
/// the tailer runs only when watching is enabled, at least one guild is active,
/// and a logs folder is set; called after any change to those inputs (SSE
/// active-set updates, the watching toggle, the folder picker). logs the
/// session start/stop transitions.
pub fn reconcile(app: &AppHandle) {
    let state = app.state::<crate::AppState>();
    let settings = crate::settings::Settings::load(
        &app.path()
            .app_config_dir()
            .map(|d| d.join("settings.json"))
            .unwrap_or_default(),
    );
    let enabled = settings.livelog_watching_enabled();
    let folder = settings.logs_folder.clone();
    let has_active = !state.combatlog.active_guilds.lock_safe().is_empty();
    let should_run = enabled && has_active && folder.is_some();

    // hold the tailer lock across the whole start/stop decision so concurrent
    // reconciles (the SSE reader thread plus command threads) can't both act on a
    // stale view and double-spawn or strand a tailer.
    let mut tailer = state.combatlog.tailer.lock_safe();
    let running = tailer.is_some();
    if should_run && !running {
        let stop = Arc::new(AtomicBool::new(false));
        *tailer = Some(stop.clone());
        start(app.clone(), folder.expect("checked is_some"), stop);
        crate::applog::push(app, "info", format!("{TAG}live session started — watching the combat log"));
    } else if !should_run && running {
        // take the flag out before signalling it, so a start racing right behind
        // this stop sees running == false and spawns a brand-new tailer with a
        // fresh flag instead of being suppressed by the one that's exiting.
        if let Some(stop) = tailer.take() {
            stop.store(true, Ordering::Relaxed);
        }
        // clear server presence ONLY on an actual disable — not on session-end or a
        // restart (where `enabled` stays true). that way the lone `false` beat can
        // never reorder past a fresh `true` from a tailer starting right behind us;
        // session-end presence simply ages out via the server TTL.
        if !enabled {
            spawn_heartbeat(false);
        }
        crate::applog::push(app, "info", format!("{TAG}live session ended — stopped watching"));
    }
}

/// spawn the tailer thread with its own `stop` flag (created and stored by
/// reconcile under the tailer lock). the thread exits when that flag is set;
/// reconcile has already cleared the handle by then, so there's nothing to
/// surrender on exit.
fn start(app: AppHandle, folder: String, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        run(&app, &folder, &stop);
    });
}

/// the tailer loop: open the live file at its end, then poll for appended bytes,
/// parsing only ENCOUNTER_* / CHALLENGE_MODE_* lines. handles truncation,
/// rotation, and a file that doesn't exist yet (logged once).
fn run(app: &AppHandle, folder: &str, stop: &Arc<AtomicBool>) {
    let mut file: Option<std::fs::File> = None;
    let mut open_path: Option<PathBuf> = None;
    let mut offset: u64 = 0;
    // a partial trailing line held (as raw bytes) until its newline arrives.
    let mut pending: Vec<u8> = Vec::new();
    // whether we're inside a CHALLENGE_MODE (m+) — encounter ends are ignored
    // while true.
    let mut in_challenge = false;
    // log "combat log not found" at most once per run so a missing file doesn't
    // spam the activity log every poll.
    let mut warned_missing = false;
    // when we last beat the server (only while a log file is actually open).
    let mut last_hb: Option<Instant> = None;

    while !stop.load(Ordering::Relaxed) {
        // (re)open if we have no handle or the chosen path changed/rotated.
        let target = find_log_file(folder);
        match (&target, &open_path) {
            (Some(t), Some(o)) if t == o => {}
            (Some(t), _) => {
                match std::fs::File::open(t) {
                    Ok(mut f) => {
                        // seek to end so we never parse history.
                        offset = f.seek(SeekFrom::End(0)).unwrap_or(0);
                        file = Some(f);
                        open_path = Some(t.clone());
                        pending.clear();
                        warned_missing = false;
                        // confirm pickup (and rotations) in the activity log — a new
                        // file appearing mid-session lands here on the next poll.
                        let fname = t.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                        crate::applog::push(app, "info", format!("{TAG}now watching {fname}"));
                        // beat right away on open so a pull ending in the first ~20s
                        // is still recognized as companion-driven (not just at +20s).
                        spawn_heartbeat(true);
                        last_hb = Some(Instant::now());
                    }
                    Err(_) => {
                        file = None;
                        open_path = None;
                    }
                }
            }
            (None, _) => {
                if !warned_missing {
                    crate::applog::push(app, "warn", format!("{TAG}combat log not found in {folder}"));
                    warned_missing = true;
                }
                file = None;
                open_path = None;
            }
        }

        if let Some(f) = file.as_mut() {
            // detect truncation/rotation by size vs. our offset.
            let size = f.metadata().map(|m| m.len()).unwrap_or(offset);
            if size < offset {
                // truncated in place — restart from the top.
                offset = 0;
                pending.clear();
            }
            if size > offset {
                // read forward in bounded slices: a heavy mythic pull appends
                // megabytes between polls, and feeding it all as one giant chunk
                // is what let the tailer fall behind. each slice keeps feed()
                // cheap; the inner loop catches up fully before the next sleep.
                const MAX_READ: usize = 4 * 1024 * 1024;
                if f.seek(SeekFrom::Start(offset)).is_ok() {
                    while offset < size && !stop.load(Ordering::Relaxed) {
                        let want = ((size - offset) as usize).min(MAX_READ);
                        let mut buf = vec![0u8; want];
                        match f.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                buf.truncate(n);
                                offset += n as u64;
                                feed(app, &mut pending, &buf, &mut in_challenge);
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        }

        // heartbeat: only while we actually have the combat log open — that trio
        // (livelog enabled + live session + log found) is the "it's working"
        // signal that earns the 5-min poll backstop. when the file isn't found we
        // stay quiet and the server-side presence ages out.
        if open_path.is_some() {
            if last_hb.map(|t| t.elapsed() >= HEARTBEAT_INTERVAL).unwrap_or(true) {
                spawn_heartbeat(true);
                last_hb = Some(Instant::now());
            }
        } else {
            last_hb = None;
        }

        sleep_until_stop(stop, POLL_MS);
    }
}

/// fire-and-forget liveness beat, off the tail loop's thread so a slow POST never
/// delays log reads. watching=true while tailing the log, false to clear presence.
fn spawn_heartbeat(watching: bool) {
    std::thread::spawn(move || {
        if let Some(token) = crate::keychain::get_token() {
            let _ = ApiClient::production(token).post_heartbeat(watching);
        }
    });
}

/// sleep ~`ms` in short slices so a stop request is noticed promptly.
fn sleep_until_stop(stop: &Arc<AtomicBool>, ms: u64) {
    let mut left = ms;
    while left > 0 && !stop.load(Ordering::Relaxed) {
        let step = left.min(100);
        std::thread::sleep(Duration::from_millis(step));
        left -= step;
    }
}

/// pick the combat-log file to tail: the most-recently-modified WoWCombatLog*.txt
/// in the folder. modern WoW writes a fresh per-session file named
/// WoWCombatLog-MMDDYY_HHMMSS.txt; older setups append to a single
/// WoWCombatLog.txt. newest-by-mtime tails whichever file is actually being
/// written right now and follows the rotation when WoW opens a new session file.
/// we deliberately do NOT prefer the bare name — a stale leftover WoWCombatLog.txt
/// would otherwise shadow the live timestamped file and we'd tail a dead file.
fn find_log_file(folder: &str) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(folder).ok()?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !(name.starts_with("WoWCombatLog") && name.ends_with(".txt")) {
            continue;
        }
        // fall back to UNIX_EPOCH when the mtime is briefly unreadable (a file WoW
        // just created can stat-fail for an instant) so we still consider it as a
        // candidate instead of skipping it until the next poll. >= so a later entry
        // with an equal (e.g. epoch) time still wins, never leaving us with none.
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mtime >= *t).unwrap_or(true) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

/// append a freshly-read byte chunk, then process every complete (newline-
/// terminated) line in ONE pass: split the buffer once and drain the processed
/// prefix once, so a big chunk costs O(n), not O(n²). (the old code drained each
/// line off the front, re-shifting the whole tail every line — quadratic, which
/// let the tailer fall hopelessly behind during high-volume mythic raids.) the
/// trailing partial line stays buffered. working on raw bytes means a chunk
/// boundary landing mid-utf8 can't corrupt a line — only whole lines are decoded.
fn feed(app: &AppHandle, pending: &mut Vec<u8>, buf: &[u8], in_challenge: &mut bool) {
    pending.extend_from_slice(buf);
    let Some(last_nl) = pending.iter().rposition(|&b| b == b'\n') else { return };
    for line_bytes in pending[..last_nl].split(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(line_bytes);
        process_line(app, line.trim_end_matches('\r'), in_challenge);
    }
    pending.drain(..=last_nl);
}

/// cheap pre-filter then dispatch a single combat-log line. only ENCOUNTER_* /
/// CHALLENGE_MODE_* lines are inspected; everything else is ignored.
fn process_line(app: &AppHandle, line: &str, in_challenge: &mut bool) {
    if !(line.contains("ENCOUNTER_") || line.contains("CHALLENGE_MODE_")) {
        return;
    }
    // "M/D/YYYY H:MM:SS.ffff" + TWO spaces + body. split on the double space; the
    // body's first comma field is the event name.
    let Some((ts, body)) = line.split_once("  ") else { return };
    let event = body.split(',').next().unwrap_or("").trim();
    match event {
        "CHALLENGE_MODE_START" => *in_challenge = true,
        "CHALLENGE_MODE_END" => *in_challenge = false,
        "ENCOUNTER_START" => {
            if *in_challenge {
                return;
            }
            if let Some(name) = encounter_name(body) {
                crate::applog::push(app, "info", format!("{TAG}encounter started: {name}"));
            }
        }
        "ENCOUNTER_END" => {
            if *in_challenge {
                // an m+ run — not a raid encounter, ignore.
                return;
            }
            handle_encounter_end(app, ts, body);
        }
        _ => {}
    }
}

/// the encounter name is the quoted second field of an ENCOUNTER_* body.
fn encounter_name(body: &str) -> Option<String> {
    let start = body.find('"')? + 1;
    let end = body[start..].find('"')? + start;
    Some(body[start..end].to_string())
}

/// fields after the closing quote of the name, comma-separated. for
/// ENCOUNTER_END these are: difficultyID, groupSize, success(0/1), fightTimeMs.
/// returns them in order so callers can index by fixed position.
fn fields_after_name(body: &str) -> Vec<&str> {
    let Some(open) = body.find('"') else { return Vec::new() };
    let Some(rel_close) = body[open + 1..].find('"') else { return Vec::new() };
    let after = &body[open + 1 + rel_close + 1..];
    // drop the leading comma that separates the name from the next field.
    after.trim_start_matches(',').split(',').map(|s| s.trim()).collect()
}

/// the encounterID is the first comma field after the event name (before the
/// quoted name), e.g. ENCOUNTER_END,3182,"…",…
fn encounter_id(body: &str) -> Option<i64> {
    body.split(',').nth(1)?.trim().parse().ok()
}

/// a qualifying ENCOUNTER_END (not in a challenge mode): signal every
/// active-livelog guild so the server can poll WCL immediately.
fn handle_encounter_end(app: &AppHandle, ts: &str, body: &str) {
    let Some(encounter_id) = encounter_id(body) else { return };
    let name = encounter_name(body).unwrap_or_else(|| encounter_id.to_string());
    // difficultyID, groupSize, success, fightTimeMs.
    let fields = fields_after_name(body);
    let difficulty_id: i64 = fields.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let success = fields.get(2).map(|s| *s == "1").unwrap_or(false);
    let fight_time_ms: i64 = fields.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);

    // local timestamps are informational in Phase 1 — best-effort conversion.
    let end_time = parse_log_ts_ms(ts).unwrap_or(0);
    let start_time = if end_time > 0 && fight_time_ms > 0 {
        end_time - fight_time_ms
    } else {
        end_time
    };

    let guilds = app.state::<crate::AppState>().combatlog.active_guilds();
    if guilds.is_empty() {
        return;
    }
    crate::applog::push(
        app,
        "info",
        format!("{TAG}encounter ended: {name} ({}) — notifying server", if success { "kill" } else { "wipe" }),
    );

    let token = match crate::keychain::get_token() {
        Some(t) => t,
        None => return,
    };
    let client = ApiClient::production(token);
    for guild_id in guilds {
        let payload = EncounterSignal {
            guild_id: guild_id.clone(),
            encounter_id,
            encounter_name: name.clone(),
            difficulty_id,
            success,
            start_time,
            end_time,
        };
        match client.post_encounter_signal(&payload) {
            Ok(resp) => {
                let msg = match (resp.tracked, resp.polled) {
                    (true, true) => format!("{TAG}server polling WCL for the new pull"),
                    (true, false) => format!("{TAG}duplicate signal — server already polling"),
                    (false, _) => format!("{TAG}{name} not a tracked raid encounter — ignored"),
                };
                crate::applog::push(app, "info", msg);
            }
            Err(e) => crate::applog::push(app, "warn", format!("{TAG}couldn't notify the server: {e}")),
        }
    }
}

/// convert a combat-log timestamp ("M/D/YYYY H:MM:SS.ffff", machine local time)
/// to epoch milliseconds UTC. best-effort: returns None on any parse failure so
/// the caller falls back to 0 rather than blocking.
fn parse_log_ts_ms(ts: &str) -> Option<i64> {
    use chrono::{Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
    let (date, time) = ts.trim().split_once(' ')?;
    let mut d = date.split('/');
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    let year: i32 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hour: u32 = t.next()?.parse().ok()?;
    let minute: u32 = t.next()?.parse().ok()?;
    // bind first: `unwrap_or`'s arg is eager, so passing `t.next()?` inline would
    // advance the iterator a second time when there's no fractional part.
    let sec_field = t.next()?;
    let (sec, frac) = sec_field.split_once('.').unwrap_or((sec_field, "0"));
    let second: u32 = sec.parse().ok()?;
    // the fractional part is milliseconds (3) or finer (e.g. .ffff); take the
    // first 3 digits as ms.
    let ms: u32 = format!("{frac:0<3}")[..3].parse().ok()?;
    let naive = NaiveDateTime::new(
        NaiveDate::from_ymd_opt(year, month, day)?,
        NaiveTime::from_hms_milli_opt(hour, minute, second, ms)?,
    );
    // interpret the civil time in the machine's local zone (DST-aware), then to
    // epoch ms. a DST gap/fold returns the earliest valid instant.
    Local.from_local_datetime(&naive).earliest().map(|dt| dt.timestamp_millis())
}
