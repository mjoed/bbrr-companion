//! scan the watch folder, decide which recordings are eligible (new + finished),
//! match them against every guild the user can see, and classify each into a
//! status the UI groups by (the server reports per match whether it's actually
//! uploadable).

use crate::api::{ApiClient, Guild, MatchInput, MatchResult};
use crate::metadata;
use crate::util::{mtime_ms, now_ms, STABLE_MS};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchedInfo {
    pub guild_id: String,
    pub guild_name: String,
    pub pull_id: String,
    pub boss_name: String,
    pub pull_number: i64,
    pub is_kill: bool,
    pub date: String,
    pub start_time: String,
    /// the POV belongs to one of the signed-in user's own characters.
    pub is_own_pov: bool,
    /// this user may upload it (own POV, or a guild admin uploading another's).
    pub can_upload: bool,
    pub existing_video_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoItem {
    pub id: String,
    pub path: String,
    pub filename: String,
    pub size_bytes: u64,
    pub player_name: Option<String>,
    pub player_realm: Option<String>,
    /// pending | matched | uploading | uploaded | skipped | unmatched | error
    pub status: String,
    pub reason: Option<String>,
    pub matched: Option<MatchedInfo>,
    /// bytes uploaded so far while status == "uploading".
    #[serde(default)]
    pub uploaded_bytes: Option<u64>,
}

fn status_rank(s: &str) -> u8 {
    // matches (and in-flight/done uploads) first, then everything else.
    match s {
        "matched" => 0,
        "uploading" => 1,
        "uploaded" => 2,
        "unmatched" => 3,
        "pending" => 4,
        "error" => 5,
        "skipped" => 6,
        _ => 7,
    }
}

pub fn scan_and_match(
    base: &str,
    token: &str,
    all_guilds: &[Guild],
    selected: &Option<Vec<String>>,
    folder: &str,
    prev: &[VideoItem],
    verify: bool,
) -> Result<(Vec<VideoItem>, bool), String> {
    // already-uploaded files are reused verbatim (no sidecar re-read, no re-match),
    // so a restart/re-scan only looks at new/unresolved recordings. a `verify`
    // scan skips this so a server-side POV deletion gets noticed. unmatched/pending
    // files are always re-tried (their pull may have imported since).
    let prev_uploaded: HashMap<&str, &VideoItem> = if verify {
        HashMap::new()
    } else {
        prev.iter()
            .filter(|v| v.status == "uploaded")
            .map(|v| (v.path.as_str(), v))
            .collect()
    };
    let is_selected = |gid: &str| {
        selected
            .as_ref()
            .map(|s| s.iter().any(|x| x == gid))
            .unwrap_or(true)
    };
    let guild_name = |gid: &str| {
        all_guilds
            .iter()
            .find(|g| g.id == gid)
            .map(|g| g.name.clone())
            .unwrap_or_else(|| gid.to_string())
    };

    let mut items: Vec<VideoItem> = Vec::new();
    // eligible recordings as (item, raw sidecar metadata): the VideoItem built
    // during the walk is reused at classify time instead of being rebuilt.
    let mut candidates: Vec<(VideoItem, serde_json::Value)> = Vec::new();
    let now = now_ms();

    // 1. walk for .mp4 files and decide eligibility. depth 8 covers Archon's
    //    per-pull subfolders even when the user picks a folder a few levels
    //    above `warcraft-live` (each video is paired with the sidecar in its
    //    own directory, so nesting depth doesn't affect correctness).
    for entry in WalkDir::new(folder).max_depth(8).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_mp4 = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("mp4"))
            .unwrap_or(false);
        if !is_mp4 {
            continue;
        }

        let path_str = path.to_string_lossy().to_string();
        // already uploaded — reuse the cached entry, skip all I/O + matching.
        if let Some(cached) = prev_uploaded.get(path_str.as_str()) {
            items.push((*cached).clone());
            continue;
        }
        let filename = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let mt = mtime_ms(path);

        let mut item = VideoItem {
            id: path_str.clone(),
            path: path_str.clone(),
            filename: filename.clone(),
            size_bytes: size,
            player_name: None,
            player_realm: None,
            status: "pending".into(),
            reason: None,
            matched: None,
            uploaded_bytes: None,
        };

        // finished? the sidecar is written when WarcraftRecorder finalises a
        // recording, and the file must have stopped changing.
        let sidecar = match metadata::sidecar_for(path) {
            Some(s) => s,
            None => {
                item.reason = Some("Waiting for the recording to finish…".into());
                items.push(item);
                continue;
            }
        };
        if now.saturating_sub(mt) < STABLE_MS {
            item.reason = Some("Finishing up…".into());
            items.push(item);
            continue;
        }

        let (typed, raw) = match metadata::parse_sidecar(&sidecar) {
            Ok(v) => v,
            Err(e) => {
                item.status = "error".into();
                item.reason = Some(format!("Couldn't read metadata: {e}"));
                items.push(item);
                continue;
            }
        };
        if typed.is_clip() {
            item.status = "skipped".into();
            item.reason = Some("Clip, not a full pull".into());
            item.player_name = typed.player_name();
            item.player_realm = typed.player_realm();
            items.push(item);
            continue;
        }

        // eligible: reuse the item built above (now with player + status set);
        // classification fills in the match below. `raw` rides along for the
        // match call so we don't rebuild a parallel struct.
        item.player_name = typed.player_name();
        item.player_realm = typed.player_realm();
        item.status = "unmatched".into();
        candidates.push((item, raw));
    }

    if candidates.is_empty() {
        items.sort_by(|a, b| status_rank(&a.status).cmp(&status_rank(&b.status)).then(a.filename.cmp(&b.filename)));
        // no match request was made, so we can't confirm the server's pull list.
        return Ok((items, false));
    }

    // 2. match against every guild the user can see (whoami already trimmed to
    // viewable/manageable). don't pre-filter to storage-enabled guilds: the
    // server reports per result whether each match is uploadable, so a view-only
    // guild still surfaces the match with a reason instead of a silent no-match.
    let client = ApiClient::new(base.to_string(), token.to_string());
    // match real guilds before personal: when a pull exists in both, the
    // per-video pick prefers the earliest match, so the POV lands on the shared
    // guild, not the personal copy (a video attaches to exactly one guild). a
    // stable sort keeps the server's order within each group.
    let mut ordered: Vec<&Guild> = all_guilds.iter().collect();
    ordered.sort_by_key(|g| g.is_personal);
    let match_guilds: Vec<String> = ordered.iter().map(|g| g.id.clone()).collect();

    // 3. match the batch against each guild.
    let inputs: Vec<MatchInput> = candidates
        .iter()
        .map(|(item, raw)| MatchInput { filename: item.filename.clone(), metadata: raw.clone() })
        .collect();
    let mut by_file: HashMap<String, Vec<(String, MatchResult)>> = HashMap::new();
    // capture the first request failure so a flaky/slow/erroring match call is
    // reported as such instead of silently looking like "no matching pull found".
    let mut match_error: Option<String> = None;
    // the server caps a single match request at 200 videos, but a raider's
    // recordings folder can hold far more — so chunk it (the web uploader
    // batches the same way). sending everything at once 400s the whole request.
    const MATCH_BATCH: usize = 100;
    for gid in &match_guilds {
        for chunk in inputs.chunks(MATCH_BATCH) {
            match client.match_videos(gid, chunk) {
                Ok(results) => {
                    for r in results {
                        by_file.entry(r.filename.clone()).or_default().push((gid.clone(), r));
                    }
                }
                Err(e) => {
                    match_error.get_or_insert_with(|| format!("{}: {}", guild_name(gid), e));
                }
            }
        }
    }

    // 4. classify.
    for (mut item, _raw) in candidates {
        let results = by_file.get(&item.filename);
        let matches: Vec<&(String, MatchResult)> = results
            .map(|v| v.iter().filter(|(_, r)| r.matched.is_some()).collect())
            .unwrap_or_default();

        if matches.is_empty() {
            // a per-result reason means the server was reached (genuine no-match
            // / roster miss). no result at all + a request error means the call
            // failed — surface that so it's not mistaken for "no matching pull".
            let server_reason = results.and_then(|v| v.iter().find_map(|(_, r)| r.reason.clone()));
            item.reason = Some(match server_reason.as_deref() {
                Some("player_not_in_roster") => "You weren't in this pull's roster".into(),
                Some(_) => "No matching pull found".into(),
                None => match &match_error {
                    Some(e) => format!("Match request failed — {e}"),
                    None => "No matching pull found".into(),
                },
            });
        } else {
            // prefer a selected guild; otherwise the first match.
            let chosen = matches.iter().copied().find(|(gid, _)| is_selected(gid)).unwrap_or(matches[0]);
            let (gid, r) = chosen;
            let pm = r.matched.as_ref().unwrap();
            let selected = is_selected(gid);
            item.matched = Some(MatchedInfo {
                guild_id: gid.clone(),
                guild_name: guild_name(gid),
                pull_id: pm.pull_id.clone(),
                boss_name: pm.boss_name.clone(),
                pull_number: pm.pull_number,
                is_kill: pm.is_kill,
                date: pm.date.clone(),
                start_time: pm.start_time.clone(),
                is_own_pov: r.is_own_pov,
                can_upload: r.can_upload,
                existing_video_id: r.existing_video_id.clone(),
            });
            if r.existing_video_id.is_some() {
                item.status = "uploaded".into();
                item.reason = Some("Already uploaded".into());
            } else if !selected {
                item.status = "skipped".into();
                item.reason = Some("Matched, but this guild isn't selected for upload".into());
            } else if !r.can_upload {
                // matched, but this user can't upload it here. still shown (the
                // pull was found) with a reason; never auto-uploads.
                item.status = "matched".into();
                item.reason = Some(match r.reason.as_deref() {
                    Some("no_upload_permission") => "Matched — you're not allowed to upload VODs to this guild".into(),
                    Some("no_storage") => "Matched — no upload storage is configured for this guild".into(),
                    Some("player_not_in_roster") => "Matched — you weren't in this pull's roster".into(),
                    _ if !r.is_own_pov => "Another player's POV — only a guild admin can upload it".into(),
                    _ => "Matched — you can't upload this recording".into(),
                });
            } else {
                item.status = "matched".into();
            }
        }
        items.push(item);
    }

    items.sort_by(|a, b| status_rank(&a.status).cmp(&status_rank(&b.status)).then(a.filename.cmp(&b.filename)));
    // the match request "succeeded" (server pull list is trustworthy) only if we
    // actually queried at least one guild and none of the calls errored.
    let match_ok = !match_guilds.is_empty() && match_error.is_none();
    Ok((items, match_ok))
}
