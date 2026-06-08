//! macOS CGEventTap 捕捉後端。
//!
//! 在獨立 thread 跑 CFRunLoop；callback 把 CGEvent 轉成 quickvm Event 送進 channel，
//! grab=true 時 `Drop`（吞掉本機輸入），否則 `Keep`（放行）。
//!
//! TODO: 監聽 TapDisabledByTimeout/UserInput 後 `tap.enable()` re-enable
//!       （目前 callback 極快不會 timeout，先略；production 必補）。
//! 限制：Secure Input（密碼欄）下 event tap 收不到鍵盤事件，macOS 設計使然。

use crate::InputCapture;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::display::CGDisplay;
use core_graphics::event::CGEvent;
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};
use quickvm_event::{Direction, Event, MouseButton};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;

pub struct MacCapture {
    grab: Arc<AtomicBool>,
}

impl MacCapture {
    pub fn new() -> Self {
        Self {
            grab: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl InputCapture for MacCapture {
    fn start(&mut self, tx: mpsc::UnboundedSender<Event>) -> anyhow::Result<()> {
        let grab = self.grab.clone();
        // CGEventTap + CFRunLoop 必須在自己的 thread 跑（run loop 阻塞）。
        thread::Builder::new()
            .name("quickvm-capture".into())
            .spawn(move || run_tap(tx, grab))?;
        Ok(())
    }

    fn set_grab(&mut self, grab: bool) {
        self.grab.store(grab, Ordering::SeqCst);
    }
}

fn run_tap(tx: mpsc::UnboundedSender<Event>, grab: Arc<AtomicBool>) {
    let (w, h) = screen_size();
    let events = vec![
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::ScrollWheel,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
    ];

    let tap = match CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        events,
        move |_proxy, etype, event| {
            if let Some(ev) = translate(etype, event, w, h) {
                let _ = tx.send(ev);
            }
            if grab.load(Ordering::SeqCst) {
                CallbackResult::Drop
            } else {
                CallbackResult::Keep
            }
        },
    ) {
        Ok(tap) => tap,
        Err(()) => {
            tracing::error!("CGEventTap::new 失敗 —— 需要「輔助使用 (Accessibility)」權限");
            return;
        }
    };

    let source = match tap.mach_port().create_runloop_source(0) {
        Ok(s) => s,
        Err(()) => {
            tracing::error!("create_runloop_source 失敗");
            return;
        }
    };
    let run_loop = CFRunLoop::get_current();
    unsafe {
        run_loop.add_source(&source, kCFRunLoopCommonModes);
    }
    tap.enable();
    tracing::info!(w, h, "CGEventTap 啟動，捕捉鍵鼠事件");
    CFRunLoop::run_current();
}

fn translate(etype: CGEventType, event: &CGEvent, w: f64, h: f64) -> Option<Event> {
    use CGEventType::*;
    match etype {
        MouseMoved | LeftMouseDragged | RightMouseDragged | OtherMouseDragged => {
            let p = event.location();
            Some(Event::MotionAbs {
                x: (p.x / w).clamp(0.0, 1.0),
                y: (p.y / h).clamp(0.0, 1.0),
            })
        }
        LeftMouseDown => Some(btn(MouseButton::Left, Direction::Press)),
        LeftMouseUp => Some(btn(MouseButton::Left, Direction::Release)),
        RightMouseDown => Some(btn(MouseButton::Right, Direction::Press)),
        RightMouseUp => Some(btn(MouseButton::Right, Direction::Release)),
        OtherMouseDown => Some(btn(other_button(event), Direction::Press)),
        OtherMouseUp => Some(btn(other_button(event), Direction::Release)),
        ScrollWheel => {
            let dy = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1) as i32;
            let dx = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2) as i32;
            Some(Event::Scroll {
                dx: dx * 120,
                dy: dy * 120,
            })
        }
        KeyDown => key_event(event, Direction::Press),
        KeyUp => key_event(event, Direction::Release),
        _ => None,
    }
}

fn key_event(event: &CGEvent, dir: Direction) -> Option<Event> {
    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    Some(Event::Key {
        usage: cgkeycode_to_hid(keycode)?,
        dir,
    })
}

fn btn(button: MouseButton, dir: Direction) -> Event {
    Event::Button { button, dir }
}

fn other_button(event: &CGEvent) -> MouseButton {
    match event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) {
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        _ => MouseButton::Middle,
    }
}

fn screen_size() -> (f64, f64) {
    let b = CGDisplay::main().bounds();
    (b.size.width.max(1.0), b.size.height.max(1.0))
}

/// macOS `CGKeyCode`（Carbon virtual keycode）→ USB HID Usage ID（Keyboard/Keypad Page 0x07）。
/// kVK_* 值出自 <HIToolbox/Events.h>；HID usage 出自 USB HID Usage Tables v1.21 §10。
fn cgkeycode_to_hid(keycode: u16) -> Option<u16> {
    Some(match keycode {
        0x00 => 0x04, // A
        0x0B => 0x05, // B
        0x08 => 0x06, // C
        0x02 => 0x07, // D
        0x0E => 0x08, // E
        0x03 => 0x09, // F
        0x05 => 0x0A, // G
        0x04 => 0x0B, // H
        0x22 => 0x0C, // I
        0x26 => 0x0D, // J
        0x28 => 0x0E, // K
        0x25 => 0x0F, // L
        0x2E => 0x10, // M
        0x2D => 0x11, // N
        0x1F => 0x12, // O
        0x23 => 0x13, // P
        0x0C => 0x14, // Q
        0x0F => 0x15, // R
        0x01 => 0x16, // S
        0x11 => 0x17, // T
        0x20 => 0x18, // U
        0x09 => 0x19, // V
        0x0D => 0x1A, // W
        0x07 => 0x1B, // X
        0x10 => 0x1C, // Y
        0x06 => 0x1D, // Z
        0x12 => 0x1E, // 1
        0x13 => 0x1F, // 2
        0x14 => 0x20, // 3
        0x15 => 0x21, // 4
        0x17 => 0x22, // 5
        0x16 => 0x23, // 6
        0x1A => 0x24, // 7
        0x1C => 0x25, // 8
        0x19 => 0x26, // 9
        0x1D => 0x27, // 0
        0x24 => 0x28, // Return
        0x35 => 0x29, // Escape
        0x33 => 0x2A, // Backspace (mac "delete")
        0x30 => 0x2B, // Tab
        0x31 => 0x2C, // Space
        0x1B => 0x2D, // - _
        0x18 => 0x2E, // = +
        0x21 => 0x2F, // [ {
        0x1E => 0x30, // ] }
        0x2A => 0x31, // \ | (US)
        0x29 => 0x33, // ; :
        0x27 => 0x34, // ' "
        0x32 => 0x35, // ` ~
        0x2B => 0x36, // , <
        0x2F => 0x37, // . >
        0x2C => 0x38, // / ?
        0x39 => 0x39, // CapsLock
        0x7A => 0x3A, // F1
        0x78 => 0x3B, // F2
        0x63 => 0x3C, // F3
        0x76 => 0x3D, // F4
        0x60 => 0x3E, // F5
        0x61 => 0x3F, // F6
        0x62 => 0x40, // F7
        0x64 => 0x41, // F8
        0x65 => 0x42, // F9
        0x6D => 0x43, // F10
        0x67 => 0x44, // F11
        0x6F => 0x45, // F12
        0x72 => 0x49, // Help (= PC Insert)
        0x73 => 0x4A, // Home
        0x74 => 0x4B, // PageUp
        0x75 => 0x4C, // Forward Delete
        0x77 => 0x4D, // End
        0x79 => 0x4E, // PageDown
        0x7C => 0x4F, // RightArrow
        0x7B => 0x50, // LeftArrow
        0x7D => 0x51, // DownArrow
        0x7E => 0x52, // UpArrow
        0x3B => 0xE0, // LeftControl
        0x38 => 0xE1, // LeftShift
        0x3A => 0xE2, // LeftAlt/Option
        0x37 => 0xE3, // LeftGUI/Command
        0x3E => 0xE4, // RightControl
        0x3C => 0xE5, // RightShift
        0x3D => 0xE6, // RightAlt/Option
        0x36 => 0xE7, // RightGUI/Command
        _ => return None,
    })
}
