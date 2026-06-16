# QuicKVM.app — menubar shell (macOS controller)

A tiny menubar app that wraps the quickvm **controller** (`quickvm connect`) so you
can start/stop it from the status bar instead of leaving a terminal open or killing
a process by hand.

- **LSUIElement** — no Dock icon, no Cmd-Tab entry; the menubar item is the only UI.
- **Swift shell + embedded Rust helper** — the shell (`Contents/MacOS/QuicKVM`) spawns
  the real binary (`Contents/Helpers/quickvm connect`). The helper does the CGEventTap
  capture + QUIC forwarding; the shell just supervises it.
- **One TCC subject** — Accessibility is granted to *QuicKVM.app*; the spawned helper's
  event tap is authorized under the app (responsible-process attribution), so you grant
  once and rebuilds keep it (Apple Development cert → stable Team ID).

## Build

```sh
make bundle     # cargo build --release (helper) + swift build (shell) + bundle + codesign
make install    # → /Applications/QuicKVM.app  (needed for launch-at-login + stable TCC)
make logs       # tail ~/Library/Logs/QuicKVM.log
```

Signing identity lives in `Makefile.local` (gitignored): `SIGN_ID` (Apple Development,
dev) and `DEV_ID_APP` (Developer ID, for `make release` notarized builds).

## First run

1. Launch → it prompts for **Accessibility**. Grant **QuicKVM** in
   System Settings → Privacy & Security → Accessibility.
2. Click the menubar ⚡ → **啟動**. (Once granted, the app auto-starts on launch.)

Menu: 啟動/停止 · status · 開啟 log · 開啟設定檔 · 開機時啟動 · 結束.

## Prerequisites (shared with the CLI)

- `~/.config/quickvm/config.toml` — controller config (server, screen layout, side).
- `~/.config/quickvm/secret` (chmod 600) — the PSK. A GUI app has no shell env, so the
  `QUICKVM_SECRET` env var won't reach it; the helper falls back to this file.
