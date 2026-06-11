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

`QUIC transport` · `screen-edge switching` · `HID-usage keymap` · `cursor grab + warp` · `Rust`

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
| Windows  | 🔲 TODO (`WH_*_LL` low-level hooks) | ✅ enigo (cross-platform) |

Today **Mac (controller) → Windows (controlled)** works end-to-end: edge switching, cursor grab, keyboard + modifiers, auto-reconnect. The controlled side only injects, so it needs no capture backend.

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
- **`proto`** — [postcard](https://crates.io/crates/postcard) wire format; reliable (key / button / scroll / control) vs datagram (motion) split
- **`transport`** — [quinn](https://crates.io/crates/quinn) QUIC; self-signed cert + skip-verify *(TODO: SSH-like fingerprint)*, keep-alive, stale-datagram drop
- **`capture`** — `InputCapture` trait; macOS `CGEventTap` backend with cursor grab, others stub
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

QUIC/TLS encrypts the link, but the client uses skip-verify (no cert pinning yet), so transport encryption alone wouldn't stop an unauthorized peer from connecting and injecting input — and `serve` binds all interfaces. So a **pre-shared key** (`QUICKVM_SECRET`, same on both ends) gates every connection via HMAC-SHA256 challenge-response: `serve` sends a fresh per-connection nonce, the client returns `HMAC(secret, nonce)`, `serve` verifies (constant-time) and only then accepts input. The secret never goes on the wire, and the per-connection nonce defeats replay. `quickvm` refuses to start without it. *(MITM via the skip-verify cert is still possible on a hostile path — fingerprint pinning is on the roadmap; on a trusted LAN the PSK is the meaningful gate.)*

</details>

<details>
<summary><b>Transport & latency</b></summary>

Motion goes over **unreliable datagrams** (a dropped one is self-healing — the next absolute position corrects it); keys/buttons/scroll/control go over **reliable streams** (a lost key-up is a stuck key). The controller samples its virtual cursor at a fixed **125 Hz tick** and sends only the latest position, so a Wi-Fi latency spike never flushes a backlog of stale coordinates. The runtime is multi-threaded so the QUIC driver isn't starved by the input loop.

Residual latency is dominated by the **controller's Wi-Fi link** (RTT base drifts ~5↔40 ms with occasional 100 ms spikes). The software side is tuned out; the real fix is **wired Ethernet on both ends**. If the controlled box has both, point `server` at its wired NIC to drop one wireless hop.

</details>

## Roadmap

- [x] Screen-edge enter/leave + input swallow (real KVM feel)
- [x] Cursor grab/freeze, modifiers, auto-reconnect, fixed-tick motion coalescing
- [ ] Windows capture (`WH_KEYBOARD_LL` / `WH_MOUSE_LL`) for bidirectional control
- [ ] Proportional screen mapping (portrait Mac vs landscape Windows aren't equal-ratio yet)
- [ ] SSH-like cert fingerprint trust (replace skip-verify); CC tuning (BBR / larger initial cwnd)
- [ ] `serve` as a persistent / boot-start service
- [ ] v2: virtual-HID inject (Karabiner daemon / FakerInput) to drive UAC prompts & password fields

## License

MIT
