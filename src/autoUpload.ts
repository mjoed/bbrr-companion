import type { VideoItem } from "./types";

// a recording is only auto-uploaded if its pull's raid happened within this
// window — older raids only ever sit in the list for manual upload.
export const AUTO_UPLOAD_WINDOW_MS = 14 * 24 * 60 * 60 * 1000;

// mutable bookkeeping that decides which recordings auto-upload vs. stay manual.
// kept in one plain object (not React refs) so the whole decision can be
// reasoned about — and unit-tested — without rendering anything.
export interface AutoUploadState {
  // files that were already on disk when this session started — the backlog.
  // these never auto-upload, no matter when they later match (e.g. once the
  // server starts matching, or a pull is imported). only recordings that appear
  // *after* the app is open auto-upload, which prevents a catch-up scan from
  // mass-uploading a recent backlog. seeded from the cache on mount and from the
  // first scan (disk truth), then frozen.
  preexistingFiles: Set<string>;
  // ids already auto-enqueued, so a freshly-appeared file is only auto-enqueued
  // once even if several scans see it still "matched".
  knownMatched: Set<string>;
  // pull ids known to already exist as of the baseline — the first scan that
  // successfully reached the server. a video matching a pull not in here is a
  // genuinely new pull (just imported in the web app) → auto-upload. this is
  // scan-timing-independent: it doesn't matter which scan catches the match, so
  // importing several pulls at once can't drop any of them.
  seenPulls: Set<string>;
  // true once the first trustworthy (matchOk) scan has seeded the baseline.
  pullBaselineReady: boolean;
  // true once the first scan has snapshotted the on-disk backlog.
  didSnapshot: boolean;
}

export function createAutoUploadState(): AutoUploadState {
  return {
    preexistingFiles: new Set(),
    knownMatched: new Set(),
    seenPulls: new Set(),
    pullBaselineReady: false,
    didSnapshot: false,
  };
}

// fold one scan result into the auto-upload state and return the ids that
// should be auto-enqueued this scan. pure except for mutating `state` (and
// reading `now`); it contains no React or IPC, so it can be unit-tested
// directly against the invariants below.
//
// invariants it protects (the app's riskiest logic — a bug here could
// mass-upload someone's whole backlog):
//   - the first scan snapshots every on-disk file as backlog (manual-only).
//   - the first match-ok scan only seeds the pull baseline; it uploads nothing.
//   - after the baseline, a video auto-uploads only if it's its owner's POV,
//     uploadable, from a raid in the last 14 days, and either a newly-appeared
//     file or a pull new since the baseline.
export function reconcileScan(
  next: VideoItem[],
  matchOk: boolean,
  state: AutoUploadState,
  now: number,
): string[] {
  // the first scan defines the backlog: every file already on disk now is
  // pre-existing and stays manual-only forever this session.
  if (!state.didSnapshot) {
    for (const v of next) state.preexistingFiles.add(v.id);
    state.didSnapshot = true;
  }

  const toEnqueue: string[] = [];

  // seeding the baseline needs a complete, trustworthy pull list: `matchOk` is
  // only true when every guild's match call succeeded, so a partial-failure scan
  // can't seed a baseline that's missing the failed guild's pulls — which would
  // later look "new" and auto-upload backlog. a failed/flaky match must never be
  // mistaken for "these pulls don't exist".
  if (matchOk && !state.pullBaselineReady) {
    // first trustworthy match = the baseline. record which pulls already exist
    // and upload nothing this round (it's all pre-existing). makes "matching
    // just came back online" safe: it seeds the baseline instead of
    // mass-uploading the back catalog.
    for (const v of next) if (v.matched) state.seenPulls.add(v.matched.pullId);
    state.pullBaselineReady = true;
  } else if (state.pullBaselineReady) {
    // ongoing pass. runs whenever the baseline exists — even if this scan had a
    // guild's match fail (matchOk === false). it only ever considers
    // status === "matched" videos, which by definition came from a guild that
    // did answer, and isNewPull is judged against the complete baseline. so one
    // flaky guild merely delays its own videos to the next clean scan instead of
    // suppressing auto-upload for the guilds that matched fine.
    const raidCutoff = now - AUTO_UPLOAD_WINDOW_MS;
    for (const v of next) {
      if (v.status !== "matched") continue;
      if (state.knownMatched.has(v.id)) continue; // already auto-handled
      const m = v.matched;
      if (!m || !m.isOwnPov || !m.canUpload) continue;
      if (new Date(m.startTime).getTime() < raidCutoff) continue; // raid > 14 days old
      // auto-upload only genuinely fresh things:
      //   1. a recording that just appeared on disk (new file), or
      //   2. a pull that's new since the baseline (just imported on the web).
      // backlog = an old file matching an already-seen pull → manual only.
      const isNewFile = !state.preexistingFiles.has(v.id);
      const isNewPull = !state.seenPulls.has(m.pullId);
      if (!isNewFile && !isNewPull) continue;
      state.knownMatched.add(v.id);
      toEnqueue.push(v.id);
    }
    // nb: do not graduate freshly-seen pulls into seenPulls here. when several
    // pulls are imported at once they become matchable across a few rapid scans;
    // adding a pull to seenPulls before the scan that enqueues its video would
    // make that video "not new" and silently skip it. re-upload is already
    // prevented by knownMatched and by the status flip to uploading.
  }

  // a video that is no longer in flight (e.g. its pull was deleted, so it went
  // back to unmatched) resets: drop it from the auto-handled set so a re-import
  // re-matches → re-uploads it, without needing an app restart.
  for (const v of next) {
    if (v.status !== "matched" && v.status !== "uploading" && v.status !== "uploaded") {
      state.knownMatched.delete(v.id);
    }
  }

  return toEnqueue;
}
