//! recorder sidecar (`.json`) parsing for WarcraftRecorder *and* Archon. keeps
//! the raw value to forward to the match API (the server normalizes either
//! format); the typed view here is only for local clip detection, player
//! identity, and duration. both formats are auto-detected per video.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct RecorderMeta {
    // ── WarcraftRecorder (flat) ──
    #[serde(default)]
    pub category: Option<String>,
    #[serde(rename = "clippedAt", default)]
    pub clipped_at: Option<f64>,
    #[serde(default)]
    pub player: Option<Player>,
    /// recording length in seconds (WarcraftRecorder only).
    #[serde(default)]
    pub duration: Option<f64>,

    // ── Archon (nested) ──
    /// owner's actor id; indexes into `players` below.
    #[serde(rename = "actorId", default)]
    pub actor_id: Option<i64>,
    #[serde(default)]
    pub players: Option<Vec<ArchonPlayer>>,
    #[serde(rename = "contentType", default)]
    pub content_type: Option<NamedRef>,
    #[serde(rename = "startTime", default)]
    pub start_time: Option<f64>,
    #[serde(rename = "endTime", default)]
    pub end_time: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Player {
    #[serde(rename = "_name", default)]
    pub name: Option<String>,
    #[serde(rename = "_realm", default)]
    pub realm: Option<String>,
}

/// one entry of Archon's `players[]`. the recording owner is the entry whose
/// `actor_id` equals the top-level `actorId` (Archon has no `player` object).
#[derive(Debug, Clone, Deserialize)]
pub struct ArchonPlayer {
    #[serde(rename = "actorId", default)]
    pub actor_id: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "serverName", default)]
    pub server_name: Option<String>,
}

/// a `{ "name": ... }` object — Archon's `contentType`.
#[derive(Debug, Clone, Deserialize)]
pub struct NamedRef {
    #[serde(default)]
    pub name: Option<String>,
}

impl RecorderMeta {
    /// Archon recording owner: the `players[]` entry matching the top-level actorId.
    fn archon_owner(&self) -> Option<&ArchonPlayer> {
        let id = self.actor_id?;
        self.players.as_ref()?.iter().find(|p| p.actor_id == Some(id))
    }

    /// clips and non-raid recordings never match a pull POV. WarcraftRecorder
    /// flags clips via `clippedAt` / a non-"Raids" `category`; Archon carries the
    /// recording type under `contentType.name`.
    pub fn is_clip(&self) -> bool {
        if self.clipped_at.is_some() {
            return true;
        }
        let content = self
            .category
            .as_deref()
            .or_else(|| self.content_type.as_ref().and_then(|c| c.name.as_deref()));
        content.map(|c| c != "Raids").unwrap_or(false)
    }

    pub fn player_name(&self) -> Option<String> {
        self.player
            .as_ref()
            .and_then(|p| p.name.clone())
            .or_else(|| self.archon_owner().and_then(|p| p.name.clone()))
    }

    pub fn player_realm(&self) -> Option<String> {
        self.player
            .as_ref()
            .and_then(|p| p.realm.clone())
            .or_else(|| self.archon_owner().and_then(|p| p.server_name.clone()))
    }

    /// recording length in seconds — WarcraftRecorder's `duration`, else derived
    /// from Archon's `endTime - startTime`.
    pub fn duration_seconds(&self) -> Option<f64> {
        self.duration.or_else(|| match (self.start_time, self.end_time) {
            (Some(s), Some(e)) => Some((e - s) / 1000.0),
            _ => None,
        })
    }
}

/// the `.json` sidecar for an `.mp4`. WarcraftRecorder writes a same-stem file
/// next to the video; Archon writes a `metadata.json` in the video's own folder.
pub fn sidecar_for(mp4: &Path) -> Option<PathBuf> {
    let same_stem = mp4.with_extension("json");
    if same_stem.is_file() {
        return Some(same_stem);
    }
    let metadata_json = mp4.parent()?.join("metadata.json");
    if metadata_json.is_file() {
        return Some(metadata_json);
    }
    None
}

/// parse a sidecar into (typed view, raw JSON value).
pub fn parse_sidecar(path: &Path) -> Result<(RecorderMeta, serde_json::Value), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let raw: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let typed: RecorderMeta = serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
    Ok((typed, raw))
}
