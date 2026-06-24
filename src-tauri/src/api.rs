//! Raid Review API client. blocking reqwest: the commands that call it are
//! `#[tauri::command(async)]`, so the synchronous HTTP runs off the main thread
//! and the calling code stays simple. types mirror the server's
//! JSON, renamed to snake_case on the Rust side. a few response fields are
//! deserialized for documentation only and carry a targeted #[allow(dead_code)]
//! (not a file-wide allow, which would hide real dead code).

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: Option<String>,
    #[serde(rename = "battleTag")]
    pub battle_tag: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Guild {
    pub id: String,
    pub name: String,
    #[serde(rename = "isPersonal", default)]
    pub is_personal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoAmI {
    pub user: User,
    pub guilds: Vec<Guild>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MatchInput {
    pub filename: String,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullMatch {
    #[serde(rename = "pullId")]
    pub pull_id: String,
    #[serde(rename = "bossName")]
    pub boss_name: String,
    #[serde(rename = "pullNumber")]
    pub pull_number: i64,
    #[serde(rename = "isKill")]
    pub is_kill: bool,
    #[serde(rename = "durationMs")]
    #[allow(dead_code)] // present in the API response; not consumed yet
    pub duration_ms: i64,
    pub date: String,
    #[serde(rename = "startTime")]
    pub start_time: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MatchResult {
    pub filename: String,
    #[serde(rename = "existingVideoId", default)]
    pub existing_video_id: Option<String>,
    #[serde(rename = "isOwnPov", default)]
    pub is_own_pov: bool,
    #[serde(rename = "canUpload", default)]
    pub can_upload: bool,
    #[serde(rename = "match", default)]
    pub matched: Option<PullMatch>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateUpload {
    pub object_key: String,
    pub upload_id: String,
    pub part_size: u64,
    pub part_urls: Vec<String>,
    #[serde(default)]
    pub storage_config_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // echoed by the server; not consumed yet
    pub storage_label: Option<String>,
}

/// what the companion sends when a tracked raid encounter ends — the server maps
/// it to active livelog reports and may trigger an immediate WCL poll.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncounterSignal {
    pub guild_id: String,
    pub encounter_id: i64,
    pub encounter_name: String,
    pub difficulty_id: i64,
    pub success: bool,
    pub start_time: i64,
    pub end_time: i64,
}

/// the server's verdict on an encounter signal: whether the boss is tracked, and
/// whether an immediate poll was actually triggered (vs. debounced).
#[derive(Debug, Clone, Deserialize)]
pub struct EncounterSignalResp {
    pub tracked: bool,
    pub polled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullSummary {
    pub id: String,
    pub boss_name: String,
    pub pull_number: i64,
    pub is_kill: bool,
    pub difficulty: i64,
    pub date: String,
    pub start_time: String,
}

pub struct ApiClient {
    base: String,
    token: String,
    http: reqwest::blocking::Client,
}

/// a failed part PUT, tagged with whether retrying it might succeed. transport
/// failures (connection reset/closed — reqwest's "error sending request") and
/// 5xx/429 are transient; a 4xx (bad/expired URL, checksum, auth) is permanent.
pub struct PartError {
    pub retryable: bool,
    pub msg: String,
}

/// PUT one part to its presigned URL and return the ETag (the URL is the
/// credential — no auth). the caller supplies a pooled client and a freshly-built
/// body so a failed attempt can be retried with a new one.
pub fn put_part(
    client: &reqwest::blocking::Client,
    url: &str,
    body: reqwest::blocking::Body,
) -> Result<String, PartError> {
    let res = client.put(url).body(body).send().map_err(|e| PartError {
        retryable: true,
        // strip the URL from the error: reqwest's Display embeds the request URL
        // by default, and `url` is the presigned PUT (with its X-Amz-Signature),
        // i.e. a bearer credential. msg is logged to activity.jsonl and surfaced
        // in the UI, so without_url() keeps the still-valid signature out of both.
        msg: e.without_url().to_string(),
    })?;
    let status = res.status();
    if !status.is_success() {
        return Err(PartError {
            retryable: status.is_server_error() || status.as_u16() == 429,
            msg: format!("part upload failed: {status}"),
        });
    }
    res.headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| PartError {
            retryable: false,
            msg: "part upload response missing ETag".to_string(),
        })
}

/// check the response status and deserialize the JSON body. `label` and the
/// (server-supplied) body are attached to the error so a failed call is legible
/// in the activity log.
fn read_json<T: serde::de::DeserializeOwned>(res: reqwest::blocking::Response, label: &str) -> Result<T, String> {
    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().unwrap_or_default();
        return Err(format!("{label} failed ({status}): {body}"));
    }
    res.json::<T>().map_err(|e| e.to_string())
}

/// one process-wide blocking client, reused (a cheap Arc clone) by every ApiClient.
/// reqwest's blocking client owns an inner tokio runtime; building a fresh one per
/// call meant that runtime was dropped wherever the ApiClient was dropped — and
/// when that's a Tauri async worker (the `#[tauri::command(async)]` handlers run
/// there), tokio panics: "cannot drop a runtime in a context where blocking is not
/// allowed". sharing one client keeps the runtime alive until process exit, so the
/// per-call clones drop cheaply. built once, lazily.
fn shared_http() -> &'static reqwest::blocking::Client {
    // timeouts are essential: `scan` is single-flight (UI side), so a hung request
    // with no timeout would freeze the re-scan loop until restart. large multipart
    // PUTs use a separate, untimed client (see put_part).
    static HTTP: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .user_agent(concat!("BBRRCompanion/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("failed to build http client")
    })
}

/// warm the shared client at startup, on the main thread — off any async worker, so
/// the first request doesn't pay the build cost and the build never runs inside a
/// runtime context.
pub fn warm_http_client() {
    let _ = shared_http();
}

impl ApiClient {
    pub fn new(base: String, token: String) -> Self {
        Self { base, token, http: shared_http().clone() }
    }

    /// client for the build-pinned base (see [`crate::settings::base_url`]).
    pub fn production(token: String) -> Self {
        Self::new(crate::settings::base_url().to_string(), token)
    }

    /// validate the token and fetch the user + the guilds they can upload to.
    pub fn whoami(&self) -> Result<WhoAmI, String> {
        let res = self
            .http
            .get(format!("{}/api/companion/whoami", self.base))
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| e.to_string())?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("unauthorized".into());
        }
        if !res.status().is_success() {
            return Err(format!("whoami failed: {}", res.status()));
        }
        res.json::<WhoAmI>().map_err(|e| e.to_string())
    }

    /// match a batch of videos (filename + raw recorder metadata) against the
    /// guild's pulls. the server caps a single request at 200 videos; the caller
    /// chunks larger folders.
    pub fn match_videos(&self, guild_id: &str, videos: &[MatchInput]) -> Result<Vec<MatchResult>, String> {
        #[derive(Deserialize)]
        struct Resp {
            results: Vec<MatchResult>,
        }
        let res = self
            .http
            .post(format!("{}/api/povs/match", self.base))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "guildId": guild_id, "videos": videos }))
            .send()
            .map_err(|e| e.to_string())?;
        Ok(read_json::<Resp>(res, "match")?.results)
    }

    /// tell the server a tracked raid encounter just ended so it can poll WCL
    /// immediately instead of waiting for its 60s livelog timer. the response
    /// reports whether the boss is tracked and whether a poll actually fired.
    pub fn post_encounter_signal(&self, payload: &EncounterSignal) -> Result<EncounterSignalResp, String> {
        let res = self
            .http
            .post(format!("{}/api/companion/encounter", self.base))
            .bearer_auth(&self.token)
            .json(payload)
            .send()
            .map_err(|e| e.to_string())?;
        read_json::<EncounterSignalResp>(res, "encounter signal")
    }

    /// recent pulls the user played in a guild — for the manual-match picker.
    pub fn list_pulls(&self, guild_id: &str) -> Result<Vec<PullSummary>, String> {
        #[derive(Deserialize)]
        struct Resp {
            pulls: Vec<PullSummary>,
        }
        let res = self
            .http
            .get(format!("{}/api/companion/pulls?guildId={}", self.base, urlencoding::encode(guild_id)))
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| e.to_string())?;
        Ok(read_json::<Resp>(res, "list pulls")?.pulls)
    }

    /// begin a multipart upload: returns the presigned part URLs to PUT to.
    pub fn create_upload(
        &self,
        pull_id: &str,
        player_name: &str,
        realm: Option<&str>,
        size_bytes: u64,
        filename: &str,
        storage: Option<&str>,
    ) -> Result<CreateUpload, String> {
        let res = self
            .http
            .post(format!("{}/api/povs/create-upload", self.base))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "pullId": pull_id,
                "playerName": player_name,
                "realm": realm,
                "sizeBytes": size_bytes,
                "filename": filename,
                "storage": storage,
            }))
            .send()
            .map_err(|e| e.to_string())?;
        read_json::<CreateUpload>(res, "create-upload")
    }

    /// finalise the multipart upload; returns the created PullVideo id.
    #[allow(clippy::too_many_arguments)]
    pub fn complete_upload(
        &self,
        pull_id: &str,
        player_name: &str,
        object_key: &str,
        upload_id: &str,
        parts: &[(u32, String)],
        size_bytes: u64,
        duration_s: Option<u64>,
        metadata: &serde_json::Value,
        storage_config_id: Option<&str>,
    ) -> Result<String, String> {
        #[derive(Deserialize)]
        struct Resp {
            id: String,
        }
        let parts_json: Vec<serde_json::Value> = parts
            .iter()
            .map(|(n, etag)| serde_json::json!({ "partNumber": n, "etag": etag }))
            .collect();
        let res = self
            .http
            .post(format!("{}/api/povs/complete", self.base))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "pullId": pull_id,
                "playerName": player_name,
                "objectKey": object_key,
                "uploadId": upload_id,
                "parts": parts_json,
                "sizeBytes": size_bytes,
                "durationS": duration_s,
                "metadata": metadata,
                "storageConfigId": storage_config_id,
            }))
            .send()
            .map_err(|e| e.to_string())?;
        Ok(read_json::<Resp>(res, "complete")?.id)
    }

    /// abort a multipart upload — tells the server to run S3 AbortMultipartUpload,
    /// which deletes the already-uploaded parts so nothing lingers in the bucket.
    /// returns Err if the request itself failed (caller logs it).
    pub fn abort_upload(&self, object_key: &str, upload_id: &str, storage_config_id: Option<&str>) -> Result<(), String> {
        let res = self
            .http
            .post(format!("{}/api/povs/abort", self.base))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "objectKey": object_key,
                "uploadId": upload_id,
                "storageConfigId": storage_config_id,
            }))
            .send()
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("abort returned {}", res.status()));
        }
        Ok(())
    }
}
