import { useCallback, useEffect, useRef, useState } from "react";
import { ipc } from "./ipc";
import type { VideoItem } from "./types";
import { createAutoUploadState, reconcileScan } from "./autoUpload";

export interface VideoPipeline {
  videos: VideoItem[];
  scanning: boolean;
  hasScanned: boolean;
  doScan: (verify?: boolean) => Promise<void>;
  scanDebounced: () => void;
  // forget the backlog/baseline snapshot — call on a folder switch so the new
  // folder's existing files are re-baselined as backlog, not treated as new.
  resetAutoUpload: () => void;
  // freeze everything currently on screen as "already existed" — call when the
  // user turns auto-upload ON, so the backlog seen while it was off never uploads
  // and only recordings/pulls appearing afterwards do.
  rebaselineAutoUpload: () => void;
  queue: string[];
  paused: boolean;
  uploadActive: boolean;
  speed: number | null; // smoothed bytes/sec
  enqueue: (id: string) => void;
  pause: () => Promise<void>;
  resume: () => Promise<void>;
  cancel: () => Promise<void>;
}

// owns the whole recording pipeline: the on-disk video list, the scan/match
// engine with its auto-upload gate (see autoUpload.ts), the upload queue, and
// the live Rust events that patch all three. pulled out together so the view
// stays presentational and the gating logic is isolated from the JSX.
export function useVideoPipeline(autoUploadEnabled: boolean): VideoPipeline {
  const [videos, setVideos] = useState<VideoItem[]>([]);
  const [scanning, setScanning] = useState(false);
  const [hasScanned, setHasScanned] = useState(false);
  const [paused, setPaused] = useState(false);
  const [uploadActive, setUploadActive] = useState(false);
  const [speed, setSpeed] = useState<number | null>(null); // smoothed bytes/sec
  // ordered ids the user/auto-watch has queued for upload (head = active).
  const [queue, setQueue] = useState<string[]>([]);

  const scanningRef = useRef(false);
  // set when a scan is requested while one is already running, so exactly one
  // more pass runs afterwards — otherwise rapid SSE pushes (importing several
  // pulls at once) would be dropped by the single-flight guard.
  const pendingScanRef = useRef(false);
  const debounceRef = useRef<number | null>(null);
  // last progress sample, for the speed estimate (id + bytes + timestamp).
  const speedRef = useRef<{ id: string; bytes: number; t: number } | null>(null);
  // backlog/baseline bookkeeping for the auto-upload decision (see autoUpload.ts).
  const autoRef = useRef(createAutoUploadState());
  // current auto-upload setting, in a ref so doScan reads the live value without
  // being re-created. when off, scans still match + list videos, they just never
  // auto-enqueue — manual upload and re-baseline-on-enable are handled in the view.
  const autoUploadRef = useRef(autoUploadEnabled);
  autoUploadRef.current = autoUploadEnabled;
  // latest scanned video list, for a synchronous re-baseline (see
  // rebaselineAutoUpload) without waiting on a scan.
  const videosRef = useRef<VideoItem[]>([]);
  videosRef.current = videos;

  // queue one video for upload (manual button or auto-upload). Rust always
  // (re)starts the worker on enqueue, so this also clears any pause.
  const enqueue = useCallback((id: string) => {
    setQueue((q) => (q.includes(id) ? q : [...q, id]));
    ipc.enqueueUpload(id);
    setUploadActive(true);
    setPaused(false);
  }, []);

  const doScan = useCallback(
    async (verify = false) => {
      if (scanningRef.current) { pendingScanRef.current = true; return; }
      scanningRef.current = true;
      setScanning(true);
      try {
        do {
          pendingScanRef.current = false;
          const { videos: next, matchOk } = await ipc.scan(verify);
          // a scan returns an in-flight upload with its persisted uploadedBytes (0);
          // keep the live byte count the progress events track so the bar doesn't
          // snap back to 0% when a scan lands mid-upload.
          setVideos((prev) => {
            const liveBytes = new Map(prev.map((v) => [v.id, v.uploadedBytes]));
            return next.map((v) =>
              v.status === "uploading" ? { ...v, uploadedBytes: liveBytes.get(v.id) ?? v.uploadedBytes } : v,
            );
          });
          setHasScanned(true);
          // all backlog/baseline gating lives in reconcileScan; it returns only
          // the ids that should actually auto-upload this pass. skipped entirely
          // while auto-upload is off — enabling it re-baselines (resetAutoUpload),
          // so the backlog seen while off is never swept up.
          if (autoUploadRef.current) {
            for (const id of reconcileScan(next, matchOk, autoRef.current, Date.now())) {
              enqueue(id);
            }
          }
        } while (pendingScanRef.current); // catch up on any scan dropped while busy
      } catch {
        /* not signed in / no folder — ignore */
      } finally {
        scanningRef.current = false;
        setScanning(false);
      }
    },
    [enqueue],
  );

  const scanDebounced = useCallback(() => {
    if (debounceRef.current) window.clearTimeout(debounceRef.current);
    debounceRef.current = window.setTimeout(() => doScan(), 1500);
  }, [doScan]);

  // reset the auto-upload gate (backlog snapshot + pull baseline). the next scan
  // then re-snapshots the current on-disk files as pre-existing and re-seeds the
  // baseline, uploading nothing for them.
  const resetAutoUpload = useCallback(() => {
    autoRef.current = createAutoUploadState();
  }, []);

  // re-baseline against what's on screen right now: snapshot the current
  // recordings and their matched pulls as "already existed", so when auto-upload
  // is switched on the backlog found while it was off stays manual-only and only
  // recordings/pulls that appear afterwards auto-upload. done synchronously (not
  // via a scan) so it can't race the gate flipping on.
  const rebaselineAutoUpload = useCallback(() => {
    const st = createAutoUploadState();
    for (const v of videosRef.current) {
      st.preexistingFiles.add(v.id);
      if (v.matched) st.seenPulls.add(v.matched.pullId);
    }
    st.didSnapshot = true;
    st.pullBaselineReady = true;
    autoRef.current = st;
  }, []);

  // initial load. everything in the persisted cache was on disk before this
  // session, so it's backlog (manual-only) — seed it so a live event that fires
  // before the first scan can't auto-upload an already-present recording.
  useEffect(() => {
    ipc
      .listVideos()
      .then((vs) => {
        setVideos(vs);
        for (const v of vs) autoRef.current.preexistingFiles.add(v.id);
      })
      .catch(() => {});
  }, []);

  // live upload + scan events that touch the video list, queue and speed.
  useEffect(() => {
    const subs = [
      ipc.onVideoUpdated((item) => {
        setVideos((prev) => prev.map((v) => (v.id === item.id ? item : v)));
        // a video leaves the queue once it's done, failed, or cancelled.
        if (item.status !== "uploading") {
          setQueue((q) => q.filter((id) => id !== item.id));
        }
      }),
      ipc.onUploadProgress((p) => {
        setVideos((prev) =>
          prev.map((v) => (v.id === p.id ? { ...v, status: "uploading", uploadedBytes: p.uploadedBytes } : v)),
        );
        // smoothed speed from the byte delta between progress samples (EMA).
        const now = Date.now();
        const last = speedRef.current;
        if (last && last.id === p.id && now > last.t) {
          const inst = ((p.uploadedBytes - last.bytes) * 1000) / (now - last.t);
          if (isFinite(inst) && inst >= 0) {
            setSpeed((prev) => (prev == null ? inst : prev * 0.7 + inst * 0.3));
          }
        } else {
          setSpeed(null); // new upload — restart the estimate
        }
        speedRef.current = { id: p.id, bytes: p.uploadedBytes, t: now };
      }),
      ipc.onUploadsIdle(() => {
        setUploadActive(false);
        setQueue([]);
        setSpeed(null);
        speedRef.current = null;
      }),
    ];
    return () => subs.forEach((s) => s.then((fn) => fn()));
  }, []);

  const resetSpeed = useCallback(() => {
    setSpeed(null);
    speedRef.current = null;
  }, []);
  const pause = useCallback(async () => {
    await ipc.pauseUploads();
    setPaused(true);
    resetSpeed(); // stale while paused; recomputed on resume
  }, [resetSpeed]);
  const resume = useCallback(async () => {
    await ipc.resumeUploads();
    setPaused(false);
  }, []);
  const cancel = useCallback(async () => {
    await ipc.cancelUploads();
    setPaused(false);
    setUploadActive(false);
    resetSpeed();
  }, [resetSpeed]);

  return {
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
  };
}
