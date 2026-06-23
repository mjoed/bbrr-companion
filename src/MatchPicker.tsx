import { useEffect, useState } from "react";
import { ipc } from "./ipc";
import type { Guild, PullSummary, VideoItem } from "./types";

const DIFF: Record<number, string> = { 3: "N", 4: "H", 5: "M" };

export default function MatchPicker({
  video,
  guilds,
  defaultGuildId,
  onClose,
}: {
  video: VideoItem;
  guilds: Guild[];
  defaultGuildId: string;
  onClose: () => void;
}) {
  const [guildId, setGuildId] = useState(defaultGuildId || guilds[0]?.id || "");
  const [pulls, setPulls] = useState<PullSummary[]>([]);
  const [loading, setLoading] = useState(false);
  const [filter, setFilter] = useState("");

  useEffect(() => {
    if (!guildId) return;
    setLoading(true);
    setPulls([]);
    ipc
      .listPulls(guildId)
      .then(setPulls)
      .catch(() => {})
      .finally(() => setLoading(false));
  }, [guildId]);

  const q = filter.trim().toLowerCase();
  const filtered = q ? pulls.filter((p) => p.bossName.toLowerCase().includes(q)) : pulls;

  async function pick(p: PullSummary) {
    const guildName = guilds.find((g) => g.id === guildId)?.name ?? "";
    await ipc.manualMatch(video.id, {
      guildId,
      guildName,
      pullId: p.id,
      bossName: p.bossName,
      pullNumber: p.pullNumber,
      isKill: p.isKill,
      date: p.date,
      startTime: p.startTime,
    });
    onClose();
  }

  return (
    <div className="overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="row spread">
          <div className="card-title">Match “{video.filename}”</div>
          <button className="link" onClick={onClose}>Close</button>
        </div>
        <div className="row" style={{ marginTop: "0.5rem" }}>
          <select value={guildId} onChange={(e) => setGuildId(e.target.value)} className="grow">
            {guilds.map((g) => (
              <option key={g.id} value={g.id}>{g.name}</option>
            ))}
          </select>
        </div>
        <input
          className="grow"
          style={{ marginTop: "0.5rem" }}
          placeholder="Filter by boss…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <div className="pulls">
          {loading ? (
            <p className="muted small">Loading pulls…</p>
          ) : filtered.length === 0 ? (
            <p className="muted small">No recent pulls you played in this guild.</p>
          ) : (
            filtered.map((p) => (
              <button key={p.id} className="pull-row" onClick={() => pick(p)}>
                <span className="video-name">
                  {p.bossName} {DIFF[p.difficulty] ? `(${DIFF[p.difficulty]})` : ""} — pull #{p.pullNumber}
                </span>
                <span className="muted small nowrap">
                  {new Date(p.date).toLocaleDateString()} · {p.isKill ? "kill" : "wipe"}
                </span>
              </button>
            ))
          )}
        </div>
      </div>
    </div>
  );
}
