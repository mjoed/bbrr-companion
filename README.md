# BBRR Companion

A small desktop companion for [Booty Bay Raid Review](https://www.raidreview.com). It
watches your [WarcraftRecorder](https://github.com/aza547/wow-recorder) or Archon folder,
matches new recordings to your raid pulls, and uploads them automatically — the same thing
the website's **VOD Upload** page does, but hands-free.

Built with [Tauri](https://tauri.app) (Rust core + React UI), so it's a small native app
that uses your OS's built-in webview rather than bundling a browser.

## What it does

- Sign in via raidreview.com; only existing Raid Review users can use it.
- Pick your WarcraftRecorder or Archon recording folder; the app watches it for new videos.
- For each new recording, it asks Raid Review which pull it matches (by encounter,
  difficulty, time and your character), across the guilds you choose.
- If a match is found and you have upload storage available for that guild, it uploads the
  video directly to your storage (the file never passes through Raid Review's servers).
- A clear list of every video — uploaded, matched-but-skipped, or unmatched (with manual
  match) — plus an activity log, live status, an upload speed limit, and pause/cancel.

## Windows SmartScreen warning

The installer isn't code-signed with a Microsoft/EV certificate, so on first run
Windows SmartScreen may warn about an **unknown publisher** or an unrecognized app.
This is expected and normal for a small open-source app like this — click
**More info → Run anyway** to proceed.

Updates are still cryptographically verified: the app checks a
[minisign](https://jedisct1.github.io/minisign/) signature on every downloaded
update, so you're protected against tampered binaries even without the Microsoft
signature.

## Privacy

- The only network calls are to the Raid Review API and to
  your configured storage bucket for the actual upload.
- No telemetry.

## Development

Prerequisites: [Rust](https://www.rust-lang.org/tools/install), Node.js 20+, and the
[Tauri prerequisites](https://tauri.app/start/prerequisites/) for your OS.

```bash
npm install
npm run tauri dev      # run the app in development
npm run tauri build    # produce a release installer
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the pieces fit together

## License

[MIT](LICENSE).
