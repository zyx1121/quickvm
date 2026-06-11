```
 ██████╗ ██╗   ██╗██╗ ██████╗██╗  ██╗██╗   ██╗███╗   ███╗
██╔═══██╗██║   ██║██║██╔════╝██║ ██╔╝██║   ██║████╗ ████║
██║   ██║██║   ██║██║██║     █████╔╝ ██║   ██║██╔████╔██║
██║▄▄ ██║██║   ██║██║██║     ██╔═██╗ ╚██╗ ██╔╝██║╚██╔╝██║
╚██████╔╝╚██████╔╝██║╚██████╗██║  ██╗ ╚████╔╝ ██║ ╚═╝ ██║
 ╚══▀▀═╝  ╚═════╝ ╚═╝ ╚═════╝╚═╝  ╚═╝  ╚═══╝  ╚═╝     ╚═╝
```

# quickvm

> A QUIC-based software KVM — drive your Windows box with your Mac's keyboard and mouse, switch by shoving the cursor across the screen edge. Keyboard rides a reliable stream, mouse motion rides unreliable datagrams: low latency, no TCP head-of-line blocking.

`QUIC transport` · `screen-edge switching` · `HID-usage keymap` · `cursor grab + warp` · `clipboard sync` · `Rust`

[![Rust](https://img.shields.io/badge/Rust-2024-dea584)](https://www.rust-lang.org) &nbsp;[![platform](https://img.shields.io/badge/macOS-→%20Windows-111111)](#status) &nbsp;[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](#license)

```text
        MacBook  (controller)                     Windows  (controlled)
   ┌───────────────────────────┐            ┌───────────────────────────┐
   │                           │            │                           │
   │    keyboard + mouse       │  QUIC/LAN  │    input injected here     │
   │     captured here         │  ════════▶ │                            │
   │                        ●──┼──▶ edge ───┼──▶ ●  cursor appears        │
   │                           │            │                           │
   └───────────────────────────┘            └───────────────────────────┘
         CGEventTap (grab)                          enigo (inject)

   push the cursor off the right edge ─▶ you're now driving Windows
   push it back off the left edge     ─▶ control returns to the Mac
```

<sub>The controller captures and **swallows** your input while you're on the far side, then hands it back at the reverse edge — a real KVM feel, not always-mirroring. Which edge is "across" is set in config.</sub>

## Status

| platform | capture (controller) | inject (controlled) |
|----------|----------------------|---------------------|
| macOS    | ✅ CGEventTap — mouse, keyboard, modifiers, scroll | ✅ enigo |
| Windows  | ✅ `WH_KEYBOARD_LL` / `WH_MOUSE_LL` — mouse, keyboard, scroll, warp-to-center grab | ✅ enigo (cross-platform) |

Both directions work end-to-end now: either box can be the controller (run `connect`) driving the other (`serve`), with edge switching, cursor grab, keyboard + modifiers, and auto-reconnect. The Windows controller needs no special permission (LL hooks are unprivileged); on Windows the `connect` side, like `serve`, must run in the interactive console session (session 1) or the hooks won't see real input.

## Build

Needs Rust ([rustup](https://rustup.rs)) and a C toolchain for `aws-lc-rs` — macOS: Xcode Command Line Tools; Windows: MSVC build tools + NASM + CMake.

```sh
cargo build --release
```

## Configure

The controller reads `~/.config/quickvm/config.toml` (copy [`config.example.toml`](config.example.toml)). It describes the two screens' relative position so the edge switching knows which way is "across":

```toml
server = "<controlled-ip>:7777"   # pick the controlled box's lowest-latency NIC
                                  # (wired beats Wi-Fi — see Latency below)

[local]    # this machine (the Mac controller)
width = 1080
height = 1920

[remote]   # the controlled machine (Windows)
width = 1920
height = 1080
side = "right"   # which side the remote sits on: left / right / top / bottom
                 # side = "right" → shove off the Mac's right edge to cross over,
                 #                  shove off the remote's left edge to come back
```

## Run

Both ends need the **same shared secret** in `QUICKVM_SECRET` — without it `quickvm` refuses to start (no auth would mean anyone who can reach the port owns your keyboard & mouse, see [Design notes](#design-notes)). Generate one once and use it on both machines:

```sh
export QUICKVM_SECRET="$(openssl rand -hex 32)"
```

On the **controlled** machine (Windows), inject-only:

```sh
quickvm serve --bind 0.0.0.0:7777
```

On the **controller** (macOS — needs **Accessibility** permission in System Settings → Privacy & Security):

```sh
quickvm connect                          # uses ~/.config/quickvm/config.toml
quickvm connect --server <ip>:7777       # override the configured server
```

Both machines on the same LAN. Push the cursor off the configured edge to start driving the remote; push back off the reverse edge to return.

> **macOS note:** the controller must run as a foreground session process (not over plain SSH). On Windows the `serve` side must run in the interactive console session (session 1), or injected events hit an invisible service desktop.

## Architecture

Rust workspace, layered after [lan-mouse](https://github.com/feschber/lan-mouse):

- **`event`** — platform-agnostic input model; **USB HID usage** is the key-code anchor (not OS keycodes), so all platforms map symmetrically
- **`proto`** — [postcard](https://crates.io/crates/postcard) wire format; reliable (key / button / scroll / control / clipboard) vs datagram (motion) split
- **`transport`** — [quinn](https://crates.io/crates/quinn) QUIC; persisted self-signed cert + SSH-like fingerprint pinning (TOFU), keep-alive, stale-datagram drop
- **`capture`** — `InputCapture` trait; macOS `CGEventTap` + Windows `WH_*_LL` backends (both warp-to-center grab, injected-event loop guard), others stub
- **`emulation`** — `InputEmulator` trait; [enigo](https://crates.io/crates/enigo) backend *(v2: virtual-HID for UAC prompts / password fields)*
- **`app`** — CLI: `serve` (controlled) / `connect` (controller, runs the edge-switch state machine)

## Design notes

<details>
<summary><b>Cursor grab on macOS — the hard part</b></summary>

While you're driving the remote, the controller must keep its own cursor frozen and swallow local input. macOS makes this surprisingly fiddly; this is aligned with how lan-mouse and Deskflow do it:

- **`Session`-level event tap**, not HID-level. An HID-level tap's `Drop` can't stop the cursor from moving (windowserver updates it before the tap), and the suppression interval below doesn't apply there.
- **Warp the cursor back to screen center on every move**, and **skip events whose location *is* the center** — those are the warp's own echo, and their reverse delta would otherwise cancel your real movement (cursor "won't follow").
- **`CGEventSourceSetLocalEventsSuppressionInterval(0.05)`** on a session event source. After a warp, macOS suppresses local mouse events for ~250 ms by default; at per-move warp frequency that means permanent stutter. The deprecated *global* setter does **not** apply to a tap — this was the single biggest "why is it so laggy" culprit.
- **Hardware `double` delta** for motion; grabbed events get `set_type(Null)` before `Drop` so the local app never sees a stray event.
- `CGAssociateMouseAndMouseCursorPosition(false)` returns success but is **silently a no-op in a plain CLI process** (it needs a GUI app context); `TransformProcessType` doesn't rescue it. So we don't rely on it — the warp does the work.

</details>

<details>
<summary><b>Modifiers & CapsLock</b></summary>

macOS doesn't send KeyDown/KeyUp for Shift / Control / Option / Command — it sends **`FlagsChanged`**. quickvm listens for it, reads the keycode to know which modifier, and checks the corresponding flag bit to tell press from release.

**CapsLock is a hardware toggle**: the event tap sees the event but can't stop the local LED/state from flipping (lan-mouse and Deskflow have the same limitation). While grabbed, use **Shift** to type uppercase rather than CapsLock.

</details>

<details>
<summary><b>Authentication</b></summary>

Two layers, each blocking a different attack:

- **Pre-shared key** (`QUICKVM_SECRET`, same on both ends) gates every connection via HMAC-SHA256 challenge-response: `serve` sends a fresh per-connection nonce, the client returns `HMAC(secret, nonce)`, `serve` verifies (constant-time) and only then accepts input. The secret never goes on the wire, the per-connection nonce defeats replay, and `quickvm` refuses to start without it. This blocks *unauthorized control* — `serve` binds all interfaces, so without it anyone reaching the port owns your keyboard.
- **SSH-like certificate pinning (TOFU)** replaces the old skip-verify: `serve` generates its self-signed cert once and persists it (`~/.config/quickvm/cert.der`); the client records the cert's SHA-256 on first connect (`~/.config/quickvm/known_hosts`) and refuses any mismatch after that, with real TLS signature verification behind the pin (a pinned cert is public data — without proof-of-possession anyone could replay it). This blocks *impersonation/MITM* — a fake server could otherwise harvest your keystrokes even though the PSK keeps it from controlling anything. If `serve` legitimately regenerates its cert, delete the matching `known_hosts` line to re-trust.

</details>

<details>
<summary><b>Clipboard sync — text & files</b></summary>

The clipboard follows the edge switch, both ways: crossing **into** the remote pushes the controller's clipboard to it; crossing **back** pulls whatever you copied over there. No always-on clipboard watching — macOS has no pasteboard-change callback (polling only), and syncing at the switch covers the actual KVM flow (you copy, then you shove the cursor across to paste). Same trigger Deskflow uses.

**Files** need more than the text path: a "copied file" in the system clipboard is just a path reference (`public.file-url` on macOS, `CF_HDROP` on Windows), meaningless on the other machine. So quickvm streams the file **contents** across (chunked, never fully in memory), lands them in a temp dir on the far side, and points the far clipboard at the landed copies — pasting there produces real files. File lists come straight from the platform APIs (NSPasteboard / CF_HDROP via hand-rolled Win32, since arboard does text and images only); folders are skipped (v1, same as Deskflow), 256 MiB total cap, and big transfers ride their own tagged uni stream in a background task so the switch itself never waits.

Implementation notes: every reliable uni stream starts with a tag byte — `0x00` small postcard message, `0x01` file stream (the wire change is why `HS_VERSION` is 3); the controlled side answers `Leave` by pushing its clipboard back on a server→client uni stream. Both ends keep a content fingerprint (text: content hash; files: path+size+mtime, domain-separated) and skip the send when nothing changed — re-pushing identical content would stomp the far side's clipboard history on every switch. Received filenames are sanitized to their basename (no path traversal), name collisions get ` (n)` suffixes, temp dirs are swept on startup, and any clipboard failure degrades silently rather than touching the input path. Text cap stays at 1 MiB (skipped, never truncated — half a paste is worse than none).

</details>

<details>
<summary><b>Transport & latency</b></summary>

Motion goes over **unreliable datagrams** (a dropped one is self-healing — the next absolute position corrects it); keys/buttons/scroll/control go over **reliable streams** (a lost key-up is a stuck key). The controller samples its virtual cursor at a fixed **125 Hz tick** and sends only the latest position, so a Wi-Fi latency spike never flushes a backlog of stale coordinates. The runtime is multi-threaded so the QUIC driver isn't starved by the input loop.

Residual latency is dominated by the **controller's Wi-Fi link** (RTT base drifts ~5↔40 ms with occasional 100 ms spikes). The software side is tuned out; the real fix is **wired Ethernet on both ends**. If the controlled box has both, point `server` at its wired NIC to drop one wireless hop.

</details>

## Roadmap

- [x] Screen-edge enter/leave + input swallow (real KVM feel)
- [x] Cursor grab/freeze, modifiers, auto-reconnect, fixed-tick motion coalescing
- [x] Clipboard sync (text + files, both directions, synced on edge switch)
- [x] Windows capture (`WH_KEYBOARD_LL` / `WH_MOUSE_LL`) — bidirectional control
- [ ] Clipboard: images
- [ ] CI guards build/clippy/test on macOS + Windows; edge-switch logic unit-tested
- [ ] Proportional screen mapping (portrait Mac vs landscape Windows aren't equal-ratio yet)
- [x] SSH-like cert fingerprint trust (TOFU pinning, replaces skip-verify)
- [ ] CC tuning (BBR / larger initial cwnd)
- [ ] `serve` as a persistent / boot-start service
- [ ] v2: virtual-HID inject (Karabiner daemon / FakerInput) to drive UAC prompts & password fields

## License

MIT
