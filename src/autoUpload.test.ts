import { describe, expect, test } from "vitest";
import { AUTO_UPLOAD_WINDOW_MS, createAutoUploadState, reconcileScan } from "./autoUpload";
import type { MatchedInfo, VideoItem, VideoStatus } from "./types";

// a fixed "now" keeps the 14-day auto-upload window deterministic.
const NOW = Date.parse("2026-06-23T12:00:00Z");
const RECENT = new Date(NOW - 24 * 60 * 60 * 1000).toISOString(); // 1 day ago — inside the window
const OLD = new Date(NOW - AUTO_UPLOAD_WINDOW_MS - 60_000).toISOString(); // just outside the window

function matched(pullId: string, opts: Partial<MatchedInfo> = {}): MatchedInfo {
  return {
    guildId: "g1",
    guildName: "Guild",
    pullId,
    bossName: "Boss",
    pullNumber: 1,
    isKill: false,
    date: "2026-06-23",
    startTime: RECENT,
    isOwnPov: true,
    canUpload: true,
    existingVideoId: null,
    ...opts,
  };
}

function video(id: string, status: VideoStatus, m: MatchedInfo | null = null): VideoItem {
  return {
    id,
    path: id,
    filename: `${id}.mp4`,
    sizeBytes: 1000,
    playerName: "Player",
    playerRealm: null,
    status,
    reason: null,
    matched: m,
    uploadedBytes: null,
  };
}

describe("reconcileScan — baseline seeding", () => {
  test("first clean scan seeds the baseline and uploads nothing", () => {
    const state = createAutoUploadState();
    const enqueued = reconcileScan([video("v1", "matched", matched("p1"))], true, state, NOW);
    expect(enqueued).toEqual([]);
    expect(state.pullBaselineReady).toBe(true);
    expect(state.seenPulls.has("p1")).toBe(true);
    expect(state.preexistingFiles.has("v1")).toBe(true);
  });

  test("a partial-failure first scan does NOT seed a baseline (a failed guild's pulls must not later look new)", () => {
    const state = createAutoUploadState();
    const enqueued = reconcileScan([video("v1", "matched", matched("p1"))], false, state, NOW);
    expect(enqueued).toEqual([]);
    expect(state.pullBaselineReady).toBe(false);
    expect(state.didSnapshot).toBe(true); // backlog is still snapshotted
    expect(state.preexistingFiles.has("v1")).toBe(true);
  });
});

describe("reconcileScan — auto-upload decisions after baseline", () => {
  // seed a baseline of one existing file/pull, then return the ready state.
  function seeded() {
    const state = createAutoUploadState();
    reconcileScan([video("v1", "matched", matched("p1"))], true, state, NOW);
    return state;
  }

  test("a brand-new file matching a brand-new pull auto-uploads", () => {
    const enqueued = reconcileScan(
      [video("v1", "matched", matched("p1")), video("v2", "matched", matched("p2"))],
      true,
      seeded(),
      NOW,
    );
    expect(enqueued).toEqual(["v2"]);
  });

  test("the new-pull upload still happens even when this scan had a guild fail (matchOk=false)", () => {
    const enqueued = reconcileScan(
      [video("v1", "matched", matched("p1")), video("v2", "matched", matched("p2"))],
      false,
      seeded(),
      NOW,
    );
    expect(enqueued).toEqual(["v2"]);
  });

  test("a backlog file matching an already-seen pull is never auto-uploaded", () => {
    const enqueued = reconcileScan([video("v1", "matched", matched("p1"))], true, seeded(), NOW);
    expect(enqueued).toEqual([]);
  });

  test("a new pull from a raid older than the 14-day window is not auto-uploaded", () => {
    const enqueued = reconcileScan(
      [video("vOld", "matched", matched("pOld", { startTime: OLD }))],
      true,
      seeded(),
      NOW,
    );
    expect(enqueued).toEqual([]);
  });

  test("another player's POV is never auto-uploaded", () => {
    const enqueued = reconcileScan(
      [video("vOther", "matched", matched("p2", { isOwnPov: false }))],
      true,
      seeded(),
      NOW,
    );
    expect(enqueued).toEqual([]);
  });

  test("a matched-but-not-uploadable recording is not auto-uploaded", () => {
    const enqueued = reconcileScan(
      [video("vNoUp", "matched", matched("p2", { canUpload: false }))],
      true,
      seeded(),
      NOW,
    );
    expect(enqueued).toEqual([]);
  });

  test("a fresh file is auto-uploaded only once across repeated scans", () => {
    const state = seeded();
    expect(reconcileScan([video("v2", "matched", matched("p2"))], true, state, NOW)).toEqual(["v2"]);
    // still matched on the next scan, but already handled → not re-enqueued.
    expect(reconcileScan([video("v2", "matched", matched("p2"))], true, state, NOW)).toEqual([]);
  });
});

describe("reconcileScan — backlog safety across a guild outage", () => {
  test("a guild that fails on the first scan and recovers later does NOT mass-upload its backlog", () => {
    const state = createAutoUploadState();

    // scan 1: guild B's match failed → vB shows unmatched; matchOk=false → no baseline seeded.
    expect(
      reconcileScan([video("v1", "matched", matched("p1")), video("vB", "unmatched")], false, state, NOW),
    ).toEqual([]);
    expect(state.pullBaselineReady).toBe(false);

    // scan 2: all guilds succeed → the baseline now captures both p1 and pB.
    expect(
      reconcileScan(
        [video("v1", "matched", matched("p1")), video("vB", "matched", matched("pB"))],
        true,
        state,
        NOW,
      ),
    ).toEqual([]);
    expect(state.seenPulls.has("pB")).toBe(true);

    // steady state: vB is a backlog file on a now-seen pull → it must never auto-upload.
    expect(
      reconcileScan(
        [video("v1", "matched", matched("p1")), video("vB", "matched", matched("pB"))],
        true,
        state,
        NOW,
      ),
    ).not.toContain("vB");
  });
});

describe("reconcileScan — re-eligibility after dropping out of flight", () => {
  test("a video whose pull is deleted then re-imported uploads again without a restart", () => {
    const state = createAutoUploadState();
    reconcileScan([video("v1", "matched", matched("p1"))], true, state, NOW); // baseline

    // new pull p2 + new file v2 → uploads.
    expect(reconcileScan([video("v2", "matched", matched("p2"))], true, state, NOW)).toEqual(["v2"]);
    // p2 deleted server-side → v2 goes unmatched: it's cleared from the auto-handled set.
    reconcileScan([video("v2", "unmatched")], true, state, NOW);
    // p2 re-imported → v2 matches again and re-uploads.
    expect(reconcileScan([video("v2", "matched", matched("p2"))], true, state, NOW)).toEqual(["v2"]);
  });
});

describe("reconcileScan — folder switch", () => {
  test("a fresh gate (folder switched) treats the new folder's existing files as backlog", () => {
    // running on folder A: baseline seeded, a genuinely new file uploaded.
    const a = createAutoUploadState();
    reconcileScan([video("a1", "matched", matched("pA"))], true, a, NOW);
    expect(reconcileScan([video("a2", "matched", matched("pB"))], true, a, NOW)).toEqual(["a2"]);

    // switching folders resets the gate (useVideoPipeline.resetAutoUpload). folder
    // B already has recordings on disk; the first scan must upload none of them,
    // even though they match pulls never seen on folder A.
    const b = createAutoUploadState();
    expect(
      reconcileScan([video("b1", "matched", matched("pC")), video("b2", "matched", matched("pD"))], true, b, NOW),
    ).toEqual([]);
    // a recording that appears in folder B *after* the switch does auto-upload.
    expect(
      reconcileScan(
        [video("b1", "matched", matched("pC")), video("b2", "matched", matched("pD")), video("b3", "matched", matched("pE"))],
        true,
        b,
        NOW,
      ),
    ).toEqual(["b3"]);
  });
});
