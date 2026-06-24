// mirrors the Rust command return types (see src-tauri/src/api.rs, scan.rs, settings.rs).

export interface User {
  id: string;
  name: string | null;
  battleTag: string | null;
}

export interface Guild {
  id: string;
  name: string;
  isPersonal: boolean;
}

export interface WhoAmI {
  user: User;
  guilds: Guild[];
}

export interface Settings {
  watchFolder: string | null;
  selectedGuildIds: string[] | null; // null = all guilds
  uploadLimitMbps: number | null;
  logsFolder: string | null;
  livelogWatching: boolean | null; // null = enabled (default-on)
  autoUpload: boolean | null; // null = enabled (default-on)
}

export interface MatchedInfo {
  guildId: string;
  guildName: string;
  pullId: string;
  bossName: string;
  pullNumber: number;
  isKill: boolean;
  date: string;
  startTime: string;
  isOwnPov: boolean;
  canUpload: boolean;
  existingVideoId: string | null;
}

export type VideoStatus =
  | "pending"
  | "matched"
  | "uploading"
  | "uploaded"
  | "skipped"
  | "unmatched"
  | "error";

export interface VideoItem {
  id: string;
  path: string;
  filename: string;
  sizeBytes: number;
  playerName: string | null;
  playerRealm: string | null;
  status: VideoStatus;
  reason: string | null;
  matched: MatchedInfo | null;
  uploadedBytes?: number | null;
}

export interface ScanResult {
  videos: VideoItem[];
  /** true if the server match call succeeded — i.e. the implied pull list is
   *  trustworthy enough to baseline "which pulls already existed". */
  matchOk: boolean;
}

export interface LogEntry {
  ts: number;
  level: "info" | "warn" | "error";
  message: string;
}

export interface PullSummary {
  id: string;
  bossName: string;
  pullNumber: number;
  isKill: boolean;
  difficulty: number;
  date: string;
  startTime: string;
}

export interface ManualMatchChoice {
  guildId: string;
  guildName: string;
  pullId: string;
  bossName: string;
  pullNumber: number;
  isKill: boolean;
  date: string;
  startTime: string;
}
