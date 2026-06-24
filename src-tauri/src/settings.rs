use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// persisted app settings. the server URL is intentionally not stored here — it
/// is pinned by build profile (see [`base_url`]), so a distributed build can
/// only ever reach production and the bearer token has no runtime redirect path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub watch_folder: Option<String>,
    /// guild ids to upload matched videos to. None = all the user's guilds.
    #[serde(default)]
    pub selected_guild_ids: Option<Vec<String>>,
    /// upload speed cap in MB/s. None = unlimited.
    #[serde(default)]
    pub upload_limit_mbps: Option<f64>,
    /// the WoW Logs directory containing WoWCombatLog.txt.
    #[serde(default)]
    pub logs_folder: Option<String>,
    /// combat-log livelog watching. None or true = enabled (default-on); only an
    /// explicit false disables it.
    #[serde(default)]
    pub livelog_watching: Option<bool>,
    /// auto-upload of matched recordings. None or true = enabled (default-on);
    /// only an explicit false disables it. when off, matched recordings wait for a
    /// manual upload.
    #[serde(default)]
    pub auto_upload: Option<bool>,
}

impl Settings {
    /// the effective livelog-watching flag — default-on, so only an explicit
    /// false turns it off.
    pub fn livelog_watching_enabled(&self) -> bool {
        self.livelog_watching.unwrap_or(true)
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        // write to a temp file then atomically rename (atomic on one filesystem):
        // fs::write truncates-then-writes, so a crash mid-write would leave a
        // partial settings.json that load() silently treats as "reset to defaults".
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json).map_err(|e| e.to_string())?;
        fs::rename(&tmp, path).map_err(|e| e.to_string())
    }
}

/// the Raid Review server this build talks to, hard-pinned by build profile.
/// release builds (`tauri build`) can only reach production — no UI field,
/// command, or env var changes it, so a compromised frontend can't redirect the
/// bearer token. dev builds (`tauri dev`) target the local backend.
pub fn base_url() -> &'static str {
    if cfg!(debug_assertions) {
        "http://192.168.0.9:3080"
    } else {
        "https://www.raidreview.com"
    }
}
