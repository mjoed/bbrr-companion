; Custom NSIS hooks for the BBRR Companion Windows installer.
;
; Tauri's uninstaller offers an OPTIONAL "delete application data" choice that is
; unchecked by default, so a normal uninstall leaves settings.json, videos.json,
; the activity log and the WebView cache behind. This hook removes those data
; directories unconditionally after uninstall, so removing the app always leaves
; a clean machine.
;
; Data locations (Tauri resolves both from the bundle identifier):
;   %APPDATA%\gg.raidreview.companion       — settings.json, videos.json, activity.jsonl
;   %LOCALAPPDATA%\gg.raidreview.companion  — WebView2 cache
;
; (The sign-in token lives in the Windows Credential Manager, which the
; installer cannot touch; it's a single harmless orphaned credential.)

!macro NSIS_HOOK_POSTUNINSTALL
  RMDir /r "$APPDATA\gg.raidreview.companion"
  RMDir /r "$LOCALAPPDATA\gg.raidreview.companion"
!macroend
