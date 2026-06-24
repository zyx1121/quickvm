//! macOS CGEventTap 捕捉後端。
//!
//! 在獨立 thread 跑 CFRunLoop；callback 把 CGEvent 轉成 quickvm Event 送進 channel，
//! grab=true 時 `Drop`（吞掉本機輸入），否則 `Keep`（放行）。
//!
//! TODO: 監聽 TapDisabledByTimeout/UserInput 後 `tap.enable()` re-enable
//!       （目前 callback 極快不會 timeout，先略；production 必補）。
//! 限制：Secure Input（密碼欄）下 event tap 收不到鍵盤事件，macOS 設計使然。

use crate::InputCapture;
use core_foundation::base::TCFType;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_graphics::display::CGDisplay;
use core_graphics::event::CGEvent;
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use foreign_types::ForeignType;
use quickvm_event::{Direction, Event, MouseButton};

// CoreGraphics 游標控制 C API（core-graphics crate 未提供 binding）。
// grab 期間每筆移動把游標 warp 回螢幕中心並隱藏；移動量讀硬體 double delta，warp 回響事件
// 靠 location==center 過濾（見 run_tap）。設計對齊 lan-mouse / Deskflow。
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
    fn CGWarpMouseCursorPosition(point: CGPoint) -> i32;
    /// macOS 預設 warp 後抑制本機滑鼠事件 ~250ms。grab 模式每筆移動都 warp → 永遠在抑制窗內
    /// → 移動斷續超卡。要對「特定 event source」設低值（全域 deprecated 版對 tap 不生效，
    /// 這是先前超卡的主因）。lan-mouse 實測 0.05s 合適。
    fn CGEventSourceSetLocalEventsSuppressionInterval(
        source: *const std::ffi::c_void,
        seconds: f64,
    );
    /// re-enable 被 kernel 停用的 event tap（傳 tap 的 CFMachPortRef）。
    fn CGEventTapEnable(tap: *const std::ffi::c_void, enable: bool);
}

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use tokio::sync::mpsc;

pub struct MacCapture {
    grab: Arc<AtomicBool>,
    /// 逃生旗標：tap callback 偵測到左右 Cmd chord 時 set，app 取走後交還本機。
    escape: Arc<AtomicBool>,
    /// 主螢幕像素尺寸。
    screen: (f64, f64),
}

impl MacCapture {
    pub fn new() -> Self {
        Self {
            grab: Arc::new(AtomicBool::new(false)),
            escape: Arc::new(AtomicBool::new(false)),
            screen: screen_size(),
        }
    }
}

impl Default for MacCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl InputCapture for MacCapture {
    fn start(&mut self, tx: mpsc::UnboundedSender<Event>) -> anyhow::Result<()> {
        let grab = self.grab.clone();
        let escape = self.escape.clone();
        let screen = self.screen;
        // CGEventTap + CFRunLoop 必須在自己的 thread 跑（run loop 阻塞）。
        thread::Builder::new()
            .name("quickvm-capture".into())
            .spawn(move || run_tap(tx, grab, escape, screen))?;
        Ok(())
    }

    fn set_grab(&mut self, grab: bool) {
        self.grab.store(grab, Ordering::SeqCst);
        let c = CGPoint::new(self.screen.0 / 2.0, self.screen.1 / 2.0);
        unsafe {
            // 進 / 出 grab 都先 warp 游標回中心：進 grab 是釘住的基準點；
            // 出 grab 是避免游標停在 enter 邊緣反覆觸發。
            CGWarpMouseCursorPosition(c);
            if grab {
                CGDisplayHideCursor(CGMainDisplayID());
            } else {
                CGDisplayShowCursor(CGMainDisplayID());
            }
        }
    }

    fn escape_requested(&self) -> bool {
        self.escape.swap(false, Ordering::SeqCst)
    }
}

/// 更新「目前按住的 Cmd 邊」狀態並回報這次轉換後是否構成逃生 chord（左右 Cmd 同時按住）。
///
/// FlagsChanged 只給 keycode + 當下 Command flag 是否仍 set；左右 Cmd 共用同一個 flag bit，
/// 無法只看 flag 分辨，故用 keycode 維護 2-bit 狀態（bit0 = 左 0x37、bit1 = 右 0x36）：
/// - Command flag 已清 → 兩邊都放開，歸零
/// - flag 仍 set 且該 keycode 不在集合 → 該邊剛按下，加入
/// - flag 仍 set 且該 keycode 已在集合 → 該邊放開（另一邊仍按住），移除
///
/// 回 true 當且僅當這次轉換後左右都按住 —— 即在「按下第二個 Cmd」的那一刻觸發，
/// 早於任何放開，故放開時 flag 共享造成的歧義不影響觸發正確性。
fn update_cmd_chord(state: &mut u8, keycode: u16, cmd_flag_set: bool) -> bool {
    const L: u8 = 0b01;
    const R: u8 = 0b10;
    let bit = match keycode {
        0x37 => L, // Left Command
        0x36 => R, // Right Command
        _ => return false,
    };
    if !cmd_flag_set {
        *state = 0;
        return false;
    }
    if *state & bit == 0 {
        *state |= bit;
    } else {
        *state &= !bit;
    }
    *state == (L | R)
}

fn run_tap(
    tx: mpsc::UnboundedSender<Event>,
    grab: Arc<AtomicBool>,
    escape: Arc<AtomicBool>,
    screen: (f64, f64),
) {
    // 對 session event source 設低 suppression interval：否則每筆 warp 後 ~250ms 抑制窗 → 超卡。
    // source 設完即可釋放（interval 記在 session state）；forget 跟 lan-mouse 一致避免 drop 重置。
    if let Ok(src) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) {
        unsafe { CGEventSourceSetLocalEventsSuppressionInterval(src.as_ptr() as *const _, 0.05) };
        std::mem::forget(src);
    }
    let (w, h) = screen; // 已是主螢幕尺寸（MacCapture::new 取得），不必再 screen_size()
    let (cx, cy) = (w / 2.0, h / 2.0);
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
        CGEventType::FlagsChanged, // 修飾鍵（Shift/Ctrl/Alt/Cmd）走這個，不發 KeyDown/KeyUp
    ];

    // tap 被 kernel 停用後要從 callback 內 re-enable，需要 tap 的 mach port；
    // tap 在 callback 之後才建好，故用 OnceLock 等建好再填。
    let tap_port: Arc<OnceLock<usize>> = Arc::new(OnceLock::new());
    let tap_port_cb = tap_port.clone();

    // 逃生 chord 的左右 Cmd 按住狀態。只由本 callback（單一 CFRunLoop thread）讀寫，
    // 故 load/store 已足夠，不需 CAS。
    let cmd_state = AtomicU8::new(0);

    let tap = match CGEventTap::new(
        // Session 層（非 HID）：lan-mouse / Deskflow 都用這層 —— warp / suppression / Drop
        // 在 session 層的行為才正確；HID 最底層的 Drop 擋不住游標、suppression 也不吃。
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        events,
        move |_proxy, etype, event| {
            // tap 被 kernel 停用（callback 太慢逾時 / secure input / 權限變更）—— 必須處理，
            // 否則 capture 整個停擺。timeout 可直接 re-enable；UserInput 是刻意停（密碼欄 /
            // TCC 撤銷），釋放 grab + 顯示游標避免本機卡在被吞狀態。
            match etype {
                CGEventType::TapDisabledByTimeout => {
                    if let Some(&p) = tap_port_cb.get() {
                        unsafe { CGEventTapEnable(p as *const std::ffi::c_void, true) };
                    }
                    return CallbackResult::Keep;
                }
                CGEventType::TapDisabledByUserInput => {
                    grab.store(false, Ordering::SeqCst);
                    unsafe { CGDisplayShowCursor(CGMainDisplayID()) };
                    tracing::warn!("event tap 被停用（secure input / 權限變更）—— 已釋放 grab");
                    return CallbackResult::Keep;
                }
                _ => {}
            }
            // 逃生 chord：grab 期間同時按住左右 Cmd → 就地解除 grab（完全不碰網路，
            // 對端被拿走 / 斷網 / 換網段時，這是搶回本機唯一不依賴連線狀態的路徑）。
            // FlagsChanged 才帶修飾鍵轉換；非 grab 時也追蹤狀態但不必動作（grab 已是 false）。
            if matches!(etype, CGEventType::FlagsChanged) {
                let kc = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                let cmd_set = event.get_flags().contains(CGEventFlags::CGEventFlagCommand);
                let mut s = cmd_state.load(Ordering::SeqCst);
                let chord = update_cmd_chord(&mut s, kc, cmd_set);
                cmd_state.store(s, Ordering::SeqCst);
                if chord {
                    grab.store(false, Ordering::SeqCst);
                    escape.store(true, Ordering::SeqCst);
                    cmd_state.store(0, Ordering::SeqCst);
                    unsafe { CGDisplayShowCursor(CGMainDisplayID()) };
                    tracing::warn!("逃生 chord（左右 Cmd 同時按）—— 已就地釋放 grab，交還本機");
                    return CallbackResult::Keep;
                }
            }
            let g = grab.load(Ordering::SeqCst);
            let is_move = matches!(
                etype,
                CGEventType::MouseMoved
                    | CGEventType::LeftMouseDragged
                    | CGEventType::RightMouseDragged
                    | CGEventType::OtherMouseDragged
            );
            if g && is_move {
                // grab 模式滑鼠：硬體 double delta 轉發 + warp 回中心釘住游標。
                // warp 回響事件 location 必在中心 → 過濾掉（否則其反向 delta 抵銷真實移動）；
                // 真實移動發生時游標已被移離中心，不會誤殺。
                let pos = event.location();
                if (pos.x - cx).abs() < 0.5 && (pos.y - cy).abs() < 0.5 {
                    event.set_type(CGEventType::Null);
                    return CallbackResult::Drop;
                }
                let dx = event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X);
                let dy = event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y);
                if dx != 0.0 || dy != 0.0 {
                    let _ = tx.send(Event::MotionRel { dx, dy });
                }
                unsafe { CGWarpMouseCursorPosition(CGPoint::new(cx, cy)) };
                event.set_type(CGEventType::Null); // Drop 後 CF 仍可能外傳，設 Null 確保本機收不到
                return CallbackResult::Drop;
            }
            // 其他事件（非 grab 的移動、或鍵盤 / 按鈕 / 捲動）走一般轉換。
            if let Some(ev) = translate(etype, event, w, h) {
                let _ = tx.send(ev);
            }
            if g {
                event.set_type(CGEventType::Null);
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
    // tap 建好，把 mach port 存進 OnceLock 供 callback re-enable 用。
    let _ = tap_port.set(tap.mach_port().as_concrete_TypeRef() as usize);

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
            // 非 grab 的移動才走這裡（grab 移動在 callback 用位置差分處理）。
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
            let dy =
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1) as i32;
            let dx =
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2) as i32;
            Some(Event::Scroll {
                dx: dx * 120,
                dy: dy * 120,
            })
        }
        KeyDown => key_event(event, Direction::Press),
        KeyUp => key_event(event, Direction::Release),
        // 修飾鍵：macOS 不發 KeyDown/KeyUp，發 FlagsChanged。keycode 指出哪個鍵，
        // 方向看該鍵對應的 flag bit 在事件當下是否仍 set（set=按下、clear=放開）。
        FlagsChanged => {
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let usage = cgkeycode_to_hid(keycode)?;
            let mask = modifier_flag_mask(keycode)?;
            let dir = if event.get_flags().contains(mask) {
                Direction::Press
            } else {
                Direction::Release
            };
            Some(Event::Key { usage, dir })
        }
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

/// 修飾鍵 virtual keycode → 對應的 `CGEventFlags` bit（用來從 FlagsChanged 判按下/放開）。
/// 左右同鍵共用一個 flag（macOS flags 不分左右）—— 同時按左右同修飾鍵的 edge case 暫不處理。
fn modifier_flag_mask(keycode: u16) -> Option<CGEventFlags> {
    Some(match keycode {
        0x38 | 0x3C => CGEventFlags::CGEventFlagShift, // L/R Shift
        0x3B | 0x3E => CGEventFlags::CGEventFlagControl, // L/R Control
        0x3A | 0x3D => CGEventFlags::CGEventFlagAlternate, // L/R Alt/Option
        0x37 | 0x36 => CGEventFlags::CGEventFlagCommand, // L/R Cmd
        0x39 => CGEventFlags::CGEventFlagAlphaShift,   // CapsLock
        _ => return None,
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

#[cfg(test)]
mod tests {
    use super::update_cmd_chord;

    const L: u16 = 0x37; // Left Command
    const R: u16 = 0x36; // Right Command

    #[test]
    fn chord_triggers_only_when_both_cmd_down() {
        let mut s = 0u8;
        assert!(!update_cmd_chord(&mut s, L, true)); // 左 Cmd 下
        assert!(update_cmd_chord(&mut s, R, true)); // 右 Cmd 下 → chord！
    }

    #[test]
    fn single_cmd_never_triggers() {
        let mut s = 0u8;
        // 同一邊重複 down/誤判不會湊成 chord（flag 仍 set 時 toggle）。
        assert!(!update_cmd_chord(&mut s, L, true));
        assert!(!update_cmd_chord(&mut s, L, true)); // 視為左放開 → 空
        assert!(!update_cmd_chord(&mut s, L, true)); // 又左下
        assert_eq!(s, 0b01);
    }

    #[test]
    fn chord_resets_when_all_cmd_released() {
        let mut s = 0u8;
        update_cmd_chord(&mut s, L, true);
        assert!(!update_cmd_chord(&mut s, L, false)); // Command flag 清 → 全放開
        assert_eq!(s, 0);
        assert!(!update_cmd_chord(&mut s, R, true)); // 只剩右，不成 chord
    }

    #[test]
    fn release_one_while_holding_other_keeps_the_other() {
        let mut s = 0u8;
        update_cmd_chord(&mut s, L, true); // {L}
        assert!(update_cmd_chord(&mut s, R, true)); // {L,R} chord
        // 放開左、右仍按（Command flag 仍 set）→ 該邊移除，剩右。
        assert!(!update_cmd_chord(&mut s, L, true));
        assert_eq!(s, 0b10);
    }

    #[test]
    fn non_cmd_modifier_is_ignored() {
        let mut s = 0b01; // 左 Cmd 已按
        // Shift（0x38）等非 Cmd 修飾鍵不能動到 chord 狀態。
        assert!(!update_cmd_chord(&mut s, 0x38, true));
        assert_eq!(s, 0b01);
    }
}
