# quickvm

QUIC-based software KVM — share one keyboard & mouse across machines over your LAN.
Keyboard goes over a reliable stream, mouse motion over unreliable datagrams: low latency, no TCP head-of-line blocking.

> ⚠️ **Work in progress.** macOS capture + cross-platform inject + QUIC transport work today. Windows capture and screen-edge switching are not done yet.

## Status

| platform | capture (controller) | inject (controlled) |
|----------|----------------------|---------------------|
| macOS    | ✅ CGEventTap (mouse + keyboard) | ✅ enigo |
| Windows  | 🔲 TODO (`WH_*_LL`) | ✅ enigo (cross-platform) |

So right now **Mac (controller) → Windows (controlled)** works: the controlled side only injects, it doesn't need capture.

## Build

Needs Rust ([rustup](https://rustup.rs)) and a C toolchain for `aws-lc-rs` (macOS: Xcode Command Line Tools; Windows: MSVC build tools).

```sh
cargo build --release
```

## Run

On the **controlled** machine (e.g. Windows):

```sh
quickvm serve --bind 0.0.0.0:7777
```

On the **controller** machine (e.g. macOS — needs Accessibility permission):

```sh
quickvm connect <controlled-ip>:7777
```

Move/type on the controller and it's injected on the controlled machine. Both machines must be on the same LAN.

## Architecture

Rust workspace:

- `event` — platform-agnostic input model; **USB HID usage** as the key-code anchor
- `proto` — [postcard](https://crates.io/crates/postcard) wire format; reliable (key/button/scroll/control) vs datagram (motion) split
- `transport` — [quinn](https://crates.io/crates/quinn) QUIC; self-signed cert + skip-verify (TODO: SSH-like fingerprint)
- `capture` — `InputCapture` trait; macOS CGEventTap backend, others stub
- `emulation` — `InputEmulator` trait; [enigo](https://crates.io/crates/enigo) backend (v2: virtual-HID for UAC / secure fields)
- `app` — CLI (`serve` / `connect`)

## Roadmap

- [ ] Windows capture (`WH_KEYBOARD_LL` / `WH_MOUSE_LL`)
- [ ] Screen-edge enter/leave + swallow (real KVM feel, not always-mirroring)
- [ ] Keyboard numpad / media keys; non-US layouts
- [ ] SSH-like cert fingerprint trust (replace skip-verify)
- [ ] Congestion-control tuning (BBR / larger initial cwnd for jittery Wi-Fi)
- [ ] v2: virtual-HID inject (Karabiner daemon / FakerInput) to drive UAC prompts & password fields

## License

MIT
