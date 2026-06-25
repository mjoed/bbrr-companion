import { useEffect, useRef, useState, type ReactNode } from "react";
import { ipc } from "./ipc";
import MatchPicker from "./MatchPicker";
import type { Guild, Settings, VideoItem } from "./types";
import { fmtSize, fmtSpeed, fmtEta, matchLine } from "./format";
import { useVideoPipeline } from "./useVideoPipeline";
import { useActivityLog } from "./useActivityLog";

const MATCHED_LIMIT = 20;

function VideoMeta({ v }: { v: VideoItem }) {
  return (
    <div className="muted small">
      {fmtSize(v.sizeBytes)}
      {v.playerName ? ` · POV: ${v.playerName}` : ""}
    </div>
  );
}

// collapsible "card" with a chevron header — the shared shell behind the
// activity log and each video list.
function CollapseCard({
  open,
  onToggle,
  head,
  children,
  nested,
}: {
  open: boolean;
  onToggle: () => void;
  head: ReactNode;
  children: ReactNode;
  // render as a divided sub-section (no card chrome) for nesting inside a card.
  nested?: boolean;
}) {
  return (
    <div className={nested ? "subcard" : "card"}>
      <button className="collapse-head" onClick={onToggle}>
        <span className="chev">{open ? "▾" : "▸"}</span>
        {head}
      </button>
      {open && children}
    </div>
  );
}

// the % complete for an uploading row, or null when there's nothing to show.
function uploadPct(v: VideoItem): number | null {
  return v.uploadedBytes != null && v.sizeBytes > 0
    ? Math.min(100, (v.uploadedBytes / v.sizeBytes) * 100)
    : null;
}

// the shared shell behind every video list row: filename + body on the left, an
// optional control (button / badge) on the right.
function VideoRow({ filename, children, right }: { filename: string; children?: ReactNode; right?: ReactNode }) {
  return (
    <div className="video-row">
      <div className="video-main">
        <div className="video-name">{filename}</div>
        {children}
      </div>
      {right}
    </div>
  );
}

// one row in the "matched" / "matched (other players)" lists. the two lists
// differ only in the default uploadability (`canUp`) and whether a failed
// upload may be retried when not uploadable (`retryNeedsCanUp`).
function MatchedVideoRow({
  v,
  canUp,
  retryNeedsCanUp,
  onUpload,
}: {
  v: VideoItem;
  canUp: boolean;
  retryNeedsCanUp: boolean;
  onUpload: (id: string) => void;
}) {
  const pct = v.status === "uploading" ? uploadPct(v) : null;
  const right =
    v.status === "matched" && canUp ? (
      <button className="btn primary upload-btn" onClick={() => onUpload(v.id)}>Upload</button>
    ) : v.status === "uploading" ? (
      <span className="muted small nowrap">{pct != null ? `${Math.round(pct)}%` : "…"}</span>
    ) : v.status === "error" && (!retryNeedsCanUp || canUp) ? (
      <button className="btn upload-btn" onClick={() => onUpload(v.id)}>Retry</button>
    ) : undefined;
  return (
    <VideoRow filename={v.filename} right={right}>
      <div className="muted small accent">{matchLine(v)}</div>
      <VideoMeta v={v} />
      {pct != null && (
        <div className="progress"><div className="progress-bar" style={{ width: `${pct}%` }} /></div>
      )}
      {/* own POV can still be non-uploadable (no POV-manage / no storage in
          this guild) — show why instead of an Upload button. */}
      {v.status === "error" && v.reason ? (
        <div className="small err-text">{v.reason}</div>
      ) : !canUp && v.reason ? (
        <div className="muted small">{v.reason}</div>
      ) : null}
    </VideoRow>
  );
}

export default function SignedIn({ guilds }: { guilds: Guild[] }) {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [watching, setWatching] = useState(false);
  const [limitInput, setLimitInput] = useState("");
  const [showActivity, setShowActivity] = useState(false);
  const [showMatched, setShowMatched] = useState(false);
  const [showAllMatched, setShowAllMatched] = useState(false);
  const [showMatchedOther, setShowMatchedOther] = useState(false);
  const [showAllMatchedOther, setShowAllMatchedOther] = useState(false);
  const [showExisting, setShowExisting] = useState(false);
  const [showUnmatched, setShowUnmatched] = useState(false);
  const [matchTarget, setMatchTarget] = useState<VideoItem | null>(null);
  const didAutoWatchRef = useRef(false);

  const {
    videos,
    scanning,
    hasScanned,
    doScan,
    scanDebounced,
    resetAutoUpload,
    rebaselineAutoUpload,
    queue,
    paused,
    uploadActive,
    speed,
    enqueue,
    pause,
    resume,
    cancel,
  } = useVideoPipeline(settings?.autoUpload !== false);
  const { log, showOldLog, loadingOldLog, viewOldLog } = useActivityLog();

  const allIds = guilds.map((g) => g.id);
  const isSelected = (id: string) =>
    !settings?.selectedGuildIds || settings.selectedGuildIds.includes(id);

  // initial settings load (drives the folder/guild UI and the speed-limit field).
  useEffect(() => {
    ipc.getSettings().then((s) => {
      setSettings(s);
      setLimitInput(s.uploadLimitMbps != null ? String(s.uploadLimitMbps) : "");
    });
  }, []);

  // scan when the folder is known or changes. on a *switch* (not the initial
  // load), reset the auto-upload gate first so the new folder's existing
  // recordings are re-baselined as backlog — only files that appear after the
  // switch auto-upload. passive at launch too.
  const prevFolderRef = useRef<string | null>(null);
  useEffect(() => {
    const folder = settings?.watchFolder;
    if (!folder) return;
    if (prevFolderRef.current !== null && prevFolderRef.current !== folder) {
      resetAutoUpload();
    }
    prevFolderRef.current = folder;
    doScan();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [settings?.watchFolder]);

  // auto-start watching on launch when everything is set up (folder chosen +
  // at least one guild). runs once, after the first scan has seeded the
  // known-matched baseline.
  useEffect(() => {
    if (didAutoWatchRef.current || watching) return;
    if (!settings || !hasScanned) return;
    if (!settings.watchFolder || guilds.length === 0) return;
    didAutoWatchRef.current = true;
    ipc.startWatching().then(() => setWatching(true)).catch(() => {});
  }, [settings, hasScanned, guilds, watching]);

  // while watching, re-scan on folder changes (debounced) — a live trigger, so
  // a recording that just landed and matches auto-uploads.
  useEffect(() => {
    if (!watching) return;
    let unlisten: (() => void) | undefined;
    ipc.onFsChanged(scanDebounced).then((u) => (unlisten = u));
    return () => unlisten?.();
  }, [watching, scanDebounced]);

  // server push: a pull was just created/updated for one of our guilds. live
  // re-match — a video that now resolves auto-uploads. subscribe regardless of
  // local watching state so newly-added pulls are picked up promptly.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    ipc.onPullsChanged(() => doScan()).then((u) => (unlisten = u));
    return () => unlisten?.();
  }, [doScan]);

  // SSE (re)connected — catch up on anything missed while disconnected, but
  // passively (no backlog auto-upload).
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    ipc.onEventsConnected(() => doScan()).then((u) => (unlisten = u));
    return () => unlisten?.();
  }, [doScan]);

  // poll while "finishing" files settle (no fs events once writing stops). gate
  // on a derived boolean, not the whole `videos` array, so frequent upload-progress
  // updates don't keep resetting the timer and starve the poll mid-upload.
  const hasPending = videos.some((v) => v.status === "pending");
  useEffect(() => {
    if (!watching || !hasPending) return;
    const t = window.setInterval(() => doScan(), 6000);
    return () => window.clearInterval(t);
  }, [watching, hasPending, doScan]);

  // passive backstop: SSE delivery is best-effort, so re-match occasionally in
  // case a push was missed. the live fs/SSE paths own auto-upload.
  useEffect(() => {
    if (!watching) return;
    const t = window.setInterval(() => doScan(true), 5 * 60 * 1000);
    return () => window.clearInterval(t);
  }, [watching, doScan]);

  async function chooseFolder() {
    const folder = await ipc.pickFolder();
    if (!folder) return;
    setSettings(await ipc.setFolder(folder));
  }

  async function chooseLogsFolder() {
    const folder = await ipc.pickLogsFolder();
    if (!folder) return;
    setSettings(await ipc.setLogsFolder(folder));
  }

  async function toggleLivelogWatching() {
    // default-on: null counts as enabled, so the first toggle turns it off.
    const next = settings?.livelogWatching === false;
    setSettings(await ipc.setLivelogWatching(next));
  }

  async function toggleAutoUpload() {
    // default-on: null counts as enabled, so the first toggle turns it off.
    const next = settings?.autoUpload === false;
    setSettings(await ipc.setAutoUpload(next));
    if (next) {
      // turning it ON re-baselines: the recordings/pulls found while it was off
      // are snapshotted as pre-existing, so only things discovered from now on
      // auto-upload. (anything already there stays manual-upload only.)
      rebaselineAutoUpload();
    }
  }

  async function toggleGuild(id: string) {
    const current = settings?.selectedGuildIds ?? allIds;
    const next = current.includes(id) ? current.filter((x) => x !== id) : [...current, id];
    const store = next.length === allIds.length && allIds.every((x) => next.includes(x)) ? null : next;
    setSettings(await ipc.setSelectedGuilds(store));
    doScan();
  }

  async function toggleWatching() {
    if (watching) {
      await ipc.stopWatching();
      setWatching(false);
    } else {
      await ipc.startWatching();
      setWatching(true);
      doScan(); // baseline/seenPulls gating prevents uploading the existing backlog
    }
  }

  async function saveLimit() {
    const n = parseFloat(limitInput);
    const mbps = isFinite(n) && n > 0 ? n : null;
    const saved = await ipc.setUploadLimit(mbps);
    setSettings(saved);
    setLimitInput(saved.uploadLimitMbps != null ? String(saved.uploadLimitMbps) : "");
  }

  if (!settings) return <div className="center muted">Loading…</div>;

  // newest first: matched lists by the pull's time, others by the recording's
  // filename (which is date-prefixed).
  const byPullDesc = (a: VideoItem, b: VideoItem) =>
    new Date(b.matched?.startTime ?? 0).getTime() - new Date(a.matched?.startTime ?? 0).getTime();
  const byNameDesc = (a: VideoItem, b: VideoItem) => b.filename.localeCompare(a.filename);

  // buckets, mirroring the website's POV Upload page.
  const matchedAll = videos.filter(
    (v) => v.status === "matched" || v.status === "uploading" || (v.status === "error" && v.matched),
  );
  const matched = matchedAll.filter((v) => v.matched?.isOwnPov !== false).sort(byPullDesc); // own POV
  const matchedOther = matchedAll.filter((v) => v.matched?.isOwnPov === false).sort(byPullDesc); // someone else's
  const existing = videos.filter((v) => v.status === "uploaded").sort(byPullDesc);
  const others = videos.filter((v) => !matchedAll.includes(v) && !existing.includes(v)).sort(byNameDesc);

  const uploadingVideo = videos.find((v) => v.status === "uploading");
  // the upload queue in order: head is uploading now, the rest are waiting.
  const queuedVideos = queue
    .map((id) => videos.find((v) => v.id === id))
    .filter((v): v is VideoItem => !!v);
  const active = queuedVideos[0] ?? uploadingVideo;
  const upNext = queuedVideos.slice(1);
  const shownMatched = showAllMatched ? matched : matched.slice(0, MATCHED_LIMIT);
  const shownMatchedOther = showAllMatchedOther ? matchedOther : matchedOther.slice(0, MATCHED_LIMIT);

  return (
    <section className="content">
      {/* upload control (only while something is in flight) — kept at the top so
          progress stays visible without scrolling past the video/livelog cards. */}
      {(uploadActive || queuedVideos.length > 0) && (() => {
        const pct = active ? uploadPct(active) : null;
        return (
          <div className="card">
            <div className="row spread">
              <div className="muted small">
                {paused ? "Uploads paused" : active ? `Uploading ${active.filename}` : "Uploading…"}
              </div>
              <div className="row">
                {paused ? (
                  <button className="btn primary" onClick={resume}>Resume</button>
                ) : (
                  <button className="btn ghost" onClick={pause}>Pause</button>
                )}
                <button className="btn ghost" onClick={cancel}>Cancel</button>
              </div>
            </div>
            {active && (
              <div className="row" style={{ marginTop: "0.6rem" }}>
                <div className="progress" style={{ flex: 1 }}>
                  <div className="progress-bar" style={{ width: `${pct ?? 0}%` }} />
                </div>
                <span className="muted small nowrap">{pct != null ? `${Math.round(pct)}%` : "…"}</span>
              </div>
            )}
            {active && !paused && speed != null && (
              <div className="muted small" style={{ marginTop: "0.35rem" }}>
                {fmtSpeed(speed)}
                {speed > 0 && active.uploadedBytes != null
                  ? (() => {
                      const eta = fmtEta((active.sizeBytes - active.uploadedBytes) / speed);
                      return eta ? ` · ${eta}` : "";
                    })()
                  : ""}
              </div>
            )}
            {upNext.length > 0 && (
              <div className="queue-list">
                <div className="muted small">Next up ({upNext.length})</div>
                {upNext.slice(0, 5).map((v) => (
                  <div key={v.id} className="queue-item muted small">{v.filename}</div>
                ))}
                {upNext.length > 5 && <div className="muted small">+{upNext.length - 5} more</div>}
              </div>
            )}
          </div>
        );
      })()}

      {/* automatic video upload: folder + autoupload toggle + per-guild upload
          destination (the only thing the guild picker affects, so it lives here). */}
      <div className="card">
        <div className="row spread">
          <div className="card-title" style={{ marginBottom: 0 }}>Automatic Video Upload</div>
          <label className="autostart-toggle">
            <span className={settings.autoUpload !== false ? "on" : "off"}>Autoupload</span>
            <input type="checkbox" checked={settings.autoUpload !== false} onChange={toggleAutoUpload} />
          </label>
        </div>
        <div className="muted small" style={{ marginTop: "0.75rem" }}>Select WarcraftRecorder or Archon video output folder</div>
        <div className="row spread" style={{ marginTop: "0.4rem" }}>
          <span className="path muted small">{settings.watchFolder ?? "No folder selected"}</span>
          <button className="btn ghost" onClick={chooseFolder}>Choose…</button>
        </div>
        <div className="row" style={{ marginTop: "0.75rem", justifyContent: "space-between" }}>
          <button className={`btn ${watching ? "" : "primary"}`} onClick={toggleWatching} disabled={!settings.watchFolder}>
            {watching ? "Stop watching" : "Start watching"}
          </button>
          <div className="row">
            <span className="muted small">{scanning ? "Scanning…" : watching ? "Watching" : "Idle"}</span>
            <button className="btn ghost" onClick={() => doScan(true)} disabled={!settings.watchFolder || scanning}>Scan now</button>
          </div>
        </div>
        <div className="row" style={{ marginTop: "0.75rem" }}>
          <span className="muted small">Speed limit</span>
          <input
            className="limit-input"
            value={limitInput}
            onChange={(e) => setLimitInput(e.target.value)}
            onBlur={saveLimit}
            placeholder="∞"
            inputMode="decimal"
          />
          <span className="muted small">MB/s — blank for unlimited</span>
        </div>

        {/* upload destination — only the video upload uses it, so it's nested here. */}
        <div className="muted small" style={{ marginTop: "1rem", marginBottom: "0.5rem", fontWeight: 600 }}>Upload to</div>
        {guilds.length === 0 ? (
          <p className="muted">You're not in any guild with upload access.</p>
        ) : (
          <div className="guild-toggles">
            {[...guilds].sort((a, b) => Number(a.isPersonal) - Number(b.isPersonal)).map((g) => (
              <button
                key={g.id}
                className={`guild-toggle ${isSelected(g.id) ? "on" : ""}`}
                onClick={() => toggleGuild(g.id)}
              >
                <span className="guild-name">{g.name}</span>
                {g.isPersonal && <span className="tag">personal</span>}
              </button>
            ))}
          </div>
        )}

        {/* recording lists — matched / uploaded / unmatched — grouped here since
            they all describe the recordings this card uploads. */}
        <CollapseCard
          nested
          open={showMatched}
          onToggle={() => setShowMatched((v) => !v)}
          head={`Videos matched (${matched.length})`}
        >
          <div style={{ marginTop: "0.5rem" }}>
            {watching && (
              <p className="muted small" style={{ marginBottom: "0.5rem" }}>
                New recordings upload automatically while watching. These were already here — upload them on click.
              </p>
            )}
            {matched.length === 0 ? (
              <p className="muted small">
                {settings.watchFolder ? "No matched recordings waiting to upload." : "Choose your WarcraftRecorder or Archon folder to begin."}
              </p>
            ) : (
              <>
                <div className="videos">
                  {shownMatched.map((v) => (
                    <MatchedVideoRow
                      key={v.id}
                      v={v}
                      canUp={v.matched?.canUpload !== false}
                      retryNeedsCanUp={false}
                      onUpload={enqueue}
                    />
                  ))}
                </div>
                {matched.length > MATCHED_LIMIT && (
                  <button className="view-more" onClick={() => setShowAllMatched((v) => !v)}>
                    {showAllMatched ? "Show fewer" : `View all ${matched.length}`}
                  </button>
                )}
              </>
            )}
          </div>
        </CollapseCard>

        {matchedOther.length > 0 && (
          <CollapseCard
            nested
            open={showMatchedOther}
            onToggle={() => setShowMatchedOther((v) => !v)}
            head={`Videos matched (other players) (${matchedOther.length})`}
          >
            <div style={{ marginTop: "0.5rem" }}>
              <div className="videos">
                {shownMatchedOther.map((v) => (
                  <MatchedVideoRow
                    key={v.id}
                    v={v}
                    canUp={v.matched?.canUpload === true}
                    retryNeedsCanUp={true}
                    onUpload={enqueue}
                  />
                ))}
              </div>
              {matchedOther.length > MATCHED_LIMIT && (
                <button className="view-more" onClick={() => setShowAllMatchedOther((v) => !v)}>
                  {showAllMatchedOther ? "Show fewer" : `View all ${matchedOther.length}`}
                </button>
              )}
            </div>
          </CollapseCard>
        )}

        {existing.length > 0 && (
          <CollapseCard
            nested
            open={showExisting}
            onToggle={() => setShowExisting((v) => !v)}
            head={`Uploaded (${existing.length})`}
          >
            <div className="videos" style={{ marginTop: "0.5rem" }}>
              {existing.map((v) => (
                <VideoRow key={v.id} filename={v.filename} right={<span className="badge uploaded">Uploaded</span>}>
                  <div className="muted small accent">{matchLine(v)}</div>
                  <div className="muted small">{fmtSize(v.sizeBytes)} · already on the server</div>
                </VideoRow>
              ))}
            </div>
          </CollapseCard>
        )}

        {others.length > 0 && (
          <CollapseCard
            nested
            open={showUnmatched}
            onToggle={() => setShowUnmatched((v) => !v)}
            head={`Videos without matches (${others.length})`}
          >
            <div className="videos" style={{ marginTop: "0.5rem" }}>
              {others.map((v) => (
                <VideoRow
                  key={v.id}
                  filename={v.filename}
                  right={
                    v.status === "unmatched" || v.status === "error" || v.status === "skipped" ? (
                      <button className="btn ghost match-btn" onClick={() => setMatchTarget(v)}>Match…</button>
                    ) : undefined
                  }
                >
                  <div className="muted small">
                    {v.reason ?? "No matching pull found"}
                    {v.playerName ? ` · ${v.playerName}` : ""}
                  </div>
                </VideoRow>
              ))}
            </div>
          </CollapseCard>
        )}
      </div>

      {/* livelog speedup — companion tails the combat log and pokes the web app's
          livelog poller the moment a tracked boss ends. global across all the
          user's guilds; the only control is the on/off toggle. */}
      {(() => {
        // default-on: null counts as enabled, so only an explicit false disables.
        const livelogOn = settings.livelogWatching !== false;
        const status = !livelogOn
          ? "Disabled"
          : !settings.logsFolder
            ? "Choose your WoW Logs folder to begin"
            : "Waiting for a live raid session — see the Activity log";
        return (
          <div className="card">
            <div className="row spread">
              <div className="card-title" style={{ marginBottom: 0 }}>Livelog Speedup</div>
              <label className="autostart-toggle">
                <span className={livelogOn ? "on" : "off"}>Livelog Watching</span>
                <input type="checkbox" checked={livelogOn} onChange={toggleLivelogWatching} />
              </label>
            </div>
            <div className="row spread" style={{ marginTop: "0.75rem" }}>
              <span className="path muted small">{settings.logsFolder ?? "No folder selected"}</span>
              <button className="btn ghost" onClick={chooseLogsFolder}>Browse…</button>
            </div>
            <div className="muted small" style={{ marginTop: "0.6rem" }}>{status}</div>
          </div>
        );
      })()}

      {/* activity log — newest first, collapsed by default. shows the last 24h
          unless the user loads older entries. */}
      {(() => {
        const dayAgo = Date.now() - 24 * 60 * 60 * 1000;
        const visible = showOldLog ? log : log.filter((e) => e.ts >= dayAgo);
        return (
          <CollapseCard
            open={showActivity}
            onToggle={() => setShowActivity((v) => !v)}
            head={`Activity${visible.length ? ` (${visible.length}${showOldLog ? "" : ", last 24h"})` : ""}`}
          >
            <>
              {visible.length === 0 ? (
                <p className="muted small" style={{ marginTop: "0.5rem" }}>
                  {log.length ? "Nothing in the last 24 hours." : "Nothing yet."}
                </p>
              ) : (
                <div className="logs" style={{ marginTop: "0.5rem" }}>
                  {[...visible].reverse().map((e, i) => (
                    <div key={`${e.ts}-${i}`} className={`logline ${e.level}`}>
                      <span className="muted small nowrap">{new Date(e.ts).toLocaleTimeString()}</span>
                      <span className="small">{e.message}</span>
                    </div>
                  ))}
                </div>
              )}
              {!showOldLog && (
                <button
                  className="link"
                  style={{ marginTop: "0.5rem" }}
                  onClick={viewOldLog}
                  disabled={loadingOldLog}
                >
                  {loadingOldLog ? "Loading…" : "View old entries"}
                </button>
              )}
            </>
          </CollapseCard>
        );
      })()}

      {matchTarget && (
        <MatchPicker
          video={matchTarget}
          guilds={guilds}
          defaultGuildId={settings.selectedGuildIds?.[0] ?? guilds[0]?.id ?? ""}
          onClose={() => setMatchTarget(null)}
        />
      )}
    </section>
  );
}
