import { useCallback, useEffect, useState } from "react";
import { ipc } from "./ipc";
import type { LogEntry } from "./types";

export interface ActivityLog {
  log: LogEntry[];
  showOldLog: boolean;
  loadingOldLog: boolean;
  viewOldLog: () => Promise<void>;
}

// the persisted activity log. live entries arrive via onLog; the view shows the
// last 24h by default and loads the full retained history on demand.
export function useActivityLog(): ActivityLog {
  const [log, setLog] = useState<LogEntry[]>([]);
  // the activity card shows only the last 24h by default; "view old entries"
  // loads the full persisted log (up to the 14-day retention window).
  const [showOldLog, setShowOldLog] = useState(false);
  const [loadingOldLog, setLoadingOldLog] = useState(false);

  useEffect(() => {
    ipc.getLog().then(setLog).catch(() => {});
    const sub = ipc.onLog((e) => setLog((prev) => [...prev, e].slice(-1000)));
    return () => {
      sub.then((fn) => fn());
    };
  }, []);

  const viewOldLog = useCallback(async () => {
    setLoadingOldLog(true);
    try {
      setLog(await ipc.getFullLog());
      setShowOldLog(true);
    } catch {
      /* ignore — keep showing what we have */
    } finally {
      setLoadingOldLog(false);
    }
  }, []);

  return { log, showOldLog, loadingOldLog, viewOldLog };
}
