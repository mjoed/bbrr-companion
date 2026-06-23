import type { VideoItem } from "./types";

export function fmtSize(bytes: number): string {
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(1)} GB`;
  if (bytes >= 1e6) return `${(bytes / 1e6).toFixed(0)} MB`;
  return `${(bytes / 1e3).toFixed(0)} KB`;
}

export function fmtSpeed(bytesPerSec: number): string {
  if (bytesPerSec >= 1e6) return `${(bytesPerSec / 1e6).toFixed(1)} MB/s`;
  if (bytesPerSec >= 1e3) return `${(bytesPerSec / 1e3).toFixed(0)} KB/s`;
  return `${Math.max(0, Math.round(bytesPerSec))} B/s`;
}

export function fmtEta(seconds: number): string | null {
  if (!isFinite(seconds) || seconds <= 0) return null;
  if (seconds < 60) return `~${Math.ceil(seconds)}s left`;
  if (seconds < 3600) return `~${Math.ceil(seconds / 60)}m left`;
  return `~${Math.floor(seconds / 3600)}h ${Math.ceil((seconds % 3600) / 60)}m left`;
}

export function fmtPullTime(iso: string, fallback: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return fallback;
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

export function matchLine(v: VideoItem): string | null {
  if (!v.matched) return null;
  const m = v.matched;
  return `${m.bossName} · Pull #${m.pullNumber}${m.isKill ? " (Kill)" : ""} · ${fmtPullTime(m.startTime, m.date)} · ${m.guildName}`;
}
