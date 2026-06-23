# Architecture

BBRR Companion is a [Tauri 2](https://tauri.app) app: a **Rust core** that does the native
work (file watching, reading video bytes, the throttled upload) and a **React + TypeScript
UI** that runs in the OS webview and talks to the core over Tauri's IPC.

```
┌─────────────────────────── BBRR Companion (desktop) ───────────────────────────┐
│                                                                                  │
│  React UI (webview)                         Rust core (native)                   │
│  ─ browser sign-in (PKCE)                   ─ folder watcher (notify crate)       │
│  ─ guild + folder selection                 ─ WarcraftRecorder / Archon parser    │
│  ─ video list (status per item)    ◀──IPC──▶ ─ Raid Review API client            │
│  ─ activity log + live status               ─ presigned multipart uploader        │
│  ─ speed limit, pause/cancel                  with rate limit + pause/cancel       │
│                                             ─ token storage (OS keychain)         │
└──────────────────────────────────────────────────────────────────────────────────┘
                                   │                         │
                       HTTPS (API) │                         │ HTTPS (PUT parts)
                                   ▼                         ▼
                       Raid Review  /api/*            Object storage (R2 / S3 / …)
```

Why the split: the UI is just presentation + user intent. Everything that needs the
filesystem, streams gigabytes, or must keep running while the window is in the tray lives in
Rust so it's fast and doesn't block the UI thread. The webview never talks to the network
directly — every API/storage call goes through the Rust core over IPC.

The Raid Review server is **pinned by build profile** (`settings::base_url`): release builds
can only reach `https://www.raidreview.com`, dev builds (`tauri dev`) target the local
backend. There is no runtime "server URL" setting, so a shipped build can't be pointed
elsewhere.

## Authentication

The companion never handles a Battle.net password or a browser session cookie. Sign-in is a
browser **PKCE loopback** flow (RFC 8252), implemented in `auth.rs`:

1. Generate a PKCE verifier/challenge and a CSRF `state` nonce; bind a loopback TCP listener
   on an ephemeral `127.0.0.1` port.
2. Open the system browser to `<base>/companion/authorize?…`. The user approves there.
3. The page redirects the browser to `http://127.0.0.1:<port>/callback?code=…&state=…`,
   which the core reads off the loopback socket (and checks `state` matches).
4. Exchange the one-time `code` + verifier at `POST /api/companion/token` for the real token
   (the token only ever arrives over HTTPS, never in a loopback URL).
5. Store the token in the OS keychain (Windows Credential Manager / macOS Keychain / Secret
   Service), never on disk in plain text and never in logs.

From then on the token is sent as `Authorization: Bearer <token>` on every API call. On
launch the core calls `GET /api/companion/whoami` to confirm the token is still valid and to
fetch the user + the guilds they can upload to; an invalid token drops back to the sign-in
screen.

The matching backend work for this (token model + the `/companion/*` authorize/token/whoami
endpoints + bearer acceptance on the POV routes) lives in the **main `booty-bay-raid-review`
app**, not here.

## Upload flow (per recording)

This mirrors the website's POV Upload page, calling the same endpoints:

1. **Detect** — the watcher sees a new `*.mp4`; once the file stops growing it's eligible.
2. **Parse** — read the sidecar into `RecorderMeta`. Two formats are supported: WarcraftRecorder
   (a same-basename `*.json` with `category`, `duration`, `player._name`/`_realm`, `clippedAt`)
   and Archon (a `metadata.json` in the recording's folder with `actorId`, `players[]`,
   `contentType`, `startTime`/`endTime`). Clips and non-raid content are skipped.
3. **Match** — `POST /api/povs/match` with `{ guildId, videos:[{filename, metadata}] }`,
   batched ≤100 per request, for each guild the user uploads to. The server returns the
   matched pull plus whether this user may upload it (`canUpload`/`reason`) and any
   `existingVideoId` (already on the server). A recording is matched across the selected
   guilds but attaches to **exactly one** — the selected guild if it matches, otherwise the
   first match; personal guilds are tried last so a pull shared with a real guild lands there.
4. **Upload** — `POST /api/povs/create-upload` returns presigned multipart part URLs; the
   uploader `PUT`s each ~64 MB part straight to storage (rate-limited, pause/cancellable,
   with per-part retry), then `POST /api/povs/complete`. On give-up or cancel it `POST
   /api/povs/abort`s so no orphaned multipart upload is left behind.

The file bytes go **straight from disk to the user's storage bucket**; they never pass
through the Raid Review server.

Live re-match: the core holds an SSE connection to `GET /api/povs/events`; a `pulls-changed`
push (a pull was just created/updated server-side) triggers a re-scan so a freshly-imported
pull's recording uploads promptly. A folder-change watch and a periodic backstop scan cover
the rest.

## Video state model

Each discovered file carries one status the UI groups by (`VideoStatus` in `types.ts`):

- `pending` — found on disk but still settling (writing) / not yet matched.
- `matched` — matched to a pull and uploadable; waiting to upload (or auto-uploading).
- `uploading` — in flight (drives the live progress + speed indicator).
- `uploaded` — already on the server (this run, or matched to an existing POV).
- `skipped` — a clip / non-raid recording, or matched in a guild you didn't select for upload;
   carries the reason. (the no-permission / no-storage case is `matched` with a reason and no
   Upload button.)
- `unmatched` — no pull found; the user can manually match it to one of their pulls.
- `error` — an upload failed; retryable.

## Auto-upload safety

Auto-upload must never mass-upload a backlog. The decision logic is isolated as a pure
function, `reconcileScan` in `autoUpload.ts` (no React/IPC, so it's testable): the first scan
snapshots every on-disk file as backlog (manual-only); the first fully-successful match seeds
a "pulls already seen" baseline and uploads nothing; only afterwards does a recording
auto-upload, and only if it's the user's own POV, uploadable, from a raid in the last 14 days,
and either a newly-appeared file or a pull new since the baseline.

## Multi-guild

A recording is matched against every guild the user uploads to, but uploaded to a single
guild (see "Match" above). Storage dedup of the same fight across guilds is intentionally out
of scope.

## Updates

Release builds check for an update on launch (in the Rust core, like every other
network call) and, if a newer **signed** build is published, download and install it
and restart — but never while an upload is in flight. Updates are hosted on GitHub
Releases; signature verification against an embedded public key is mandatory, so only
builds signed with the project's private key install.

## Module layout

```
src-tauri/src/
  main.rs       # binary entry → lib::run()
  lib.rs        # Tauri builder, command registration, app state, SSE listener, upload worker
  auth.rs       # browser PKCE loopback sign-in
  keychain.rs   # token storage (keyring crate)
  settings.rs   # persisted settings (folder, selected guilds, speed limit); build-pinned base URL
  watcher.rs    # folder watcher → emits "fs-changed"
  scan.rs       # recursive scan + per-guild match + upload-candidate classification
  metadata.rs   # WarcraftRecorder + Archon sidecar parsing
  api.rs        # Raid Review API client (whoami, pulls, match, create-upload, complete, abort)
  upload.rs     # presigned multipart upload: rate limit, pause/cancel, per-part retry
  applog.rs     # durable JSONL activity log
src/
  main.tsx          # React entry
  App.tsx           # shell: sign-in/out state, autostart toggle
  SignedIn.tsx      # main UI: guild/folder pickers, video lists, upload controls
  MatchPicker.tsx   # manual-match dialog
  useVideoPipeline.ts  # scan/match/upload engine hook (owns the video list + live events)
  autoUpload.ts     # the pure auto-upload gate (reconcileScan)
  useActivityLog.ts # activity-log hook
  format.ts         # display helpers
  ipc.ts            # typed wrappers over invoke()/listen()
  types.ts          # shared types mirroring the Rust JSON shapes
```
