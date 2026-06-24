// typed wrappers over the Tauri command layer (src-tauri/src/lib.rs).
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { open } from "@tauri-apps/plugin-dialog";
import { enable as autostartEnable, disable as autostartDisable, isEnabled as autostartIsEnabled } from "@tauri-apps/plugin-autostart";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { Settings, WhoAmI, VideoItem, ScanResult, LogEntry, PullSummary, ManualMatchChoice } from "./types";

export const ipc = {
  // auth
  getSettings: () => invoke<Settings>("get_settings"),
  signIn: () => invoke<WhoAmI>("sign_in"),
  cancelSignIn: () => invoke<void>("cancel_sign_in"),
  currentSession: () => invoke<WhoAmI | null>("current_session"),
  signOut: () => invoke<void>("sign_out"),
  quit: () => invoke<void>("quit_app"),

  // setup
  setFolder: (folder: string) => invoke<Settings>("set_folder", { folder }),
  setSelectedGuilds: (ids: string[] | null) => invoke<Settings>("set_selected_guilds", { ids }),

  // watch + scan
  // verify = re-check already-"uploaded" files against the server (catches a POV
  // deleted on the web). cheap scans pass false; backstop + manual scan pass true.
  scan: (verify = false) => invoke<ScanResult>("scan", { verify }),
  listVideos: () => invoke<VideoItem[]>("list_videos"),
  startWatching: () => invoke<void>("start_watching"),
  stopWatching: () => invoke<void>("stop_watching"),

  // uploads — one video at a time, user-initiated
  enqueueUpload: (videoId: string) => invoke<void>("enqueue_upload", { videoId }),
  pauseUploads: () => invoke<void>("pause_uploads"),
  resumeUploads: () => invoke<void>("resume_uploads"),
  cancelUploads: () => invoke<void>("cancel_uploads"),
  setUploadLimit: (mbps: number | null) => invoke<Settings>("set_upload_limit", { mbps }),

  // native folder picker (returns null if cancelled).
  pickFolder: () => open({ directory: true, multiple: false }) as Promise<string | null>,

  // combat-log livelog watching
  setLogsFolder: (path: string) => invoke<Settings>("set_logs_folder", { path }),
  // native picker for the WoW Logs folder (returns null if cancelled).
  pickLogsFolder: () => invoke<string | null>("pick_logs_folder"),
  setLivelogWatching: (enabled: boolean) => invoke<Settings>("set_livelog_watching", { enabled }),
  setAutoUpload: (enabled: boolean) => invoke<Settings>("set_auto_upload", { enabled }),

  // manual match
  listPulls: (guildId: string) => invoke<PullSummary[]>("list_pulls", { guildId }),
  manualMatch: (videoId: string, choice: ManualMatchChoice) => invoke<void>("manual_match", { videoId, choice }),

  // activity log
  getLog: () => invoke<LogEntry[]>("get_log"),
  getFullLog: () => invoke<LogEntry[]>("get_full_log"),
  onLog: (cb: (e: LogEntry) => void): Promise<UnlistenFn> => listen<LogEntry>("log", (ev) => cb(ev.payload)),

  // start with the OS (autostart plugin)
  autostartIsEnabled: () => autostartIsEnabled(),
  autostartEnable: () => autostartEnable(),
  autostartDisable: () => autostartDisable(),

  // app version + self-update
  appVersion: () => getVersion(),
  installUpdate: () => invoke<void>("install_update"),
  onUpdateAvailable: (cb: (version: string) => void): Promise<UnlistenFn> =>
    listen<string>("update-available", (e) => cb(e.payload)),

  // events
  onFsChanged: (cb: () => void): Promise<UnlistenFn> => listen("fs-changed", () => cb()),
  // server pushed a "pulls changed" notification (new/updated pull for a guild).
  onPullsChanged: (cb: () => void): Promise<UnlistenFn> => listen("pulls-changed", () => cb()),
  // SSE (re)connected — do a catch-up scan (passive: no auto-upload).
  onEventsConnected: (cb: () => void): Promise<UnlistenFn> => listen("events-connected", () => cb()),
  onVideoUpdated: (cb: (v: VideoItem) => void): Promise<UnlistenFn> =>
    listen<VideoItem>("video-updated", (e) => cb(e.payload)),
  onUploadProgress: (cb: (p: UploadProgress) => void): Promise<UnlistenFn> =>
    listen<UploadProgress>("upload-progress", (e) => cb(e.payload)),
  onUploadsIdle: (cb: () => void): Promise<UnlistenFn> => listen("uploads-idle", () => cb()),
};

export interface UploadProgress {
  id: string;
  uploadedBytes: number;
}
