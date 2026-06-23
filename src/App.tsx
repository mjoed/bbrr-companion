import { useEffect, useRef, useState } from "react";
import { ipc } from "./ipc";
import type { WhoAmI } from "./types";
import SignedIn from "./SignedIn";
import "./App.css";

type Status =
  | { kind: "loading" }
  | { kind: "signedOut" }
  | { kind: "signingIn" }
  | { kind: "signedIn"; who: WhoAmI }
  | { kind: "error"; message: string };

export default function App() {
  const [status, setStatus] = useState<Status>({ kind: "loading" });
  const [autostartOn, setAutostartOn] = useState(false);
  // bumped on each sign-in attempt; a stale (cancelled) attempt's result is
  // ignored so cancel can't be overridden by a late resolve/reject.
  const signInAttempt = useRef(0);
  const [version, setVersion] = useState<string | null>(null);
  const [updateVersion, setUpdateVersion] = useState<string | null>(null);
  const [updating, setUpdating] = useState(false);
  const [updateError, setUpdateError] = useState<string | null>(null);

  useEffect(() => {
    ipc.autostartIsEnabled().then(setAutostartOn).catch(() => {});
    let cancelled = false;
    // currentSession resolves to who / null; it rejects only when the server
    // can't be reached (e.g. offline right after autostart). retry a few times
    // before giving up so a boot-time network race doesn't drop a valid session
    // to the sign-in screen — an invalid token resolves to null and signs out.
    const load = (attempt: number) => {
      ipc
        .currentSession()
        .then((who) => {
          if (!cancelled) setStatus(who ? { kind: "signedIn", who } : { kind: "signedOut" });
        })
        .catch(() => {
          if (cancelled) return;
          if (attempt >= 5) setStatus({ kind: "signedOut" });
          else window.setTimeout(() => load(attempt + 1), Math.min(30000, 2000 * 2 ** attempt));
        });
    };
    load(0);
    return () => {
      cancelled = true;
    };
  }, []);

  // app version for the header + the "update available" badge. the launch check
  // installs silently when idle; this surfaces a clickable badge when a new
  // version appears while the app is already open (a long-running tray session).
  useEffect(() => {
    ipc.appVersion().then(setVersion).catch(() => {});
    let unlisten: (() => void) | undefined;
    ipc.onUpdateAvailable((v) => setUpdateVersion(v)).then((u) => (unlisten = u));
    return () => unlisten?.();
  }, []);

  async function toggleAutostart() {
    const target = !autostartOn;
    try {
      if (target) await ipc.autostartEnable();
      else await ipc.autostartDisable();
      setAutostartOn(target);
    } catch {
      // permission/plugin error — resync the toggle to the OS's actual state
      // instead of leaving it showing the value we failed to set.
      ipc.autostartIsEnabled().then(setAutostartOn).catch(() => {});
    }
  }

  async function signIn() {
    const attempt = ++signInAttempt.current;
    setStatus({ kind: "signingIn" });
    try {
      const who = await ipc.signIn();
      if (signInAttempt.current === attempt) setStatus({ kind: "signedIn", who });
    } catch (e) {
      // ignore the result of a cancelled/superseded attempt.
      if (signInAttempt.current === attempt) setStatus({ kind: "error", message: String(e) });
    }
  }

  function cancelSignIn() {
    signInAttempt.current++; // invalidate the in-flight attempt
    ipc.cancelSignIn().catch(() => {});
    setStatus({ kind: "signedOut" });
  }

  async function signOut() {
    await ipc.signOut().catch(() => {});
    setStatus({ kind: "signedOut" });
  }

  function quitApp() {
    ipc.quit().catch(() => {});
  }

  async function applyUpdate() {
    setUpdating(true);
    setUpdateError(null);
    try {
      // on success the app downloads, installs, and restarts — control usually
      // doesn't return here.
      await ipc.installUpdate();
    } catch (e) {
      setUpdating(false);
      setUpdateError(String(e));
    }
  }

  return (
    <main className="app">
      <header className="topbar">
        <div className="brand">
          <img src="/logo.svg" className="logo-img" alt="" /> BBRR Companion
          {version && <span className="app-version">v{version}</span>}
          {updateVersion && (
            <button
              className="update-badge"
              onClick={applyUpdate}
              disabled={updating}
              title={updateError ?? `Update to v${updateVersion}`}
            >
              {updating ? "Updating…" : `Update to v${updateVersion}`}
            </button>
          )}
          {updateError && <span className="small err-text">{updateError}</span>}
        </div>
        {status.kind === "signedIn" && (
          <div className="row">
            <label className="autostart-toggle">
              <span className={autostartOn ? "on" : "off"}>Autostart</span>
              <input type="checkbox" checked={autostartOn} onChange={toggleAutostart} />
            </label>
            <span className="muted small">{status.who.user.battleTag ?? status.who.user.name}</span>
            <button className="btn ghost" onClick={signOut}>Sign out</button>
            <button className="btn ghost" onClick={quitApp} title="Close the app completely (not to tray)">Quit</button>
          </div>
        )}
      </header>

      {status.kind === "loading" && <div className="center muted">Loading…</div>}

      {(status.kind === "signedOut" || status.kind === "signingIn" || status.kind === "error") && (
        <section className="center card">
          <h1>Connect to Booty Bay Raid Review</h1>
          <p className="muted">
            Sign in to your Booty Bay Raid Review account to let the companion match and upload your
            WarcraftRecorder or Archon videos.
          </p>
          <button className="btn primary" onClick={signIn} disabled={status.kind === "signingIn"}>
            {status.kind === "signingIn" ? "Waiting for approval in your browser…" : "Sign in to Raid Review"}
          </button>
          {status.kind === "signingIn" && (
            <>
              <p className="muted small">
                Approve in the browser tab that just opened. Didn&apos;t open or stuck?
              </p>
              <button className="link" onClick={cancelSignIn}>Cancel and try again</button>
            </>
          )}
          {status.kind === "error" && <p className="error">{status.message}</p>}
        </section>
      )}

      {status.kind === "signedIn" && <SignedIn guilds={status.who.guilds} />}
    </main>
  );
}
