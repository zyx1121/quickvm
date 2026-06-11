//! Windows 低階 hook 捕捉後端（WH_KEYBOARD_LL / WH_MOUSE_LL）。
//!
//! 在獨立 thread 裝兩個 LL hook 後跑 GetMessage pump（hook callback 由系統在此
//! thread context 呼叫，故 pump 必須在同 thread）。callback 是無 capture 的
//! `extern "system" fn`，靠 module-level static 取得 channel / grab 狀態。
//! grab=true 時 callback 回傳 1 吞掉本機輸入，否則 `CallNextHookEx` 放行。
//!
//! 游標凍結對齊本專案 macOS 後端的 warp-to-center：grab 期間每筆移動把游標
//! `SetCursorPos` 回螢幕中心、移動量取「相對中心」的 delta、回中心的回響事件靠
//! `pt == center` 過濾（否則反向 delta 抵銷真實移動）。Windows 無 macOS 的
//! per-source suppression-interval 問題，省去該調校。
//!
//! 限制：① 非 elevated 程序的 LL hook 收不到送往更高完整性等級（admin）視窗的
//! 輸入（UIPI）；② secure desktop（UAC 提示 / Ctrl+Alt+Del / 鎖定畫面）期間完全
//! 收不到事件 —— 與 macOS Secure Input 盲區同理。
//! TODO: LL hook callback 逾時（>LowLevelHooksTimeout，預設 ~1s）會被系統靜默
//!       移除且無通知。callback 已保持 alloc-free + 非阻塞送出；production 需補
//!       外部 watchdog 偵測 hook 失效後重裝。

use crate::InputCapture;
use quickvm_event::{Direction, Event, MouseButton};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, GetSystemMetrics, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED,
    LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, SM_CXSCREEN, SM_CYSCREEN, SetCursorPos, SetWindowsHookExW,
    ShowCursor, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

// callback 是無 capture 的 extern fn，無法挾帶 instance 狀態 —— 改用 process 層 static。
// 對齊 lan-mouse 的 Windows 後端做法；單一 capture 實例的假設下安全（quickvm 只有一個）。
static GRAB: AtomicBool = AtomicBool::new(false);
static TX: OnceLock<mpsc::UnboundedSender<Event>> = OnceLock::new();
static CENTER_X: AtomicI32 = AtomicI32::new(0);
static CENTER_Y: AtomicI32 = AtomicI32::new(0);
static SCREEN_W: AtomicI32 = AtomicI32::new(1);
static SCREEN_H: AtomicI32 = AtomicI32::new(1);

pub struct WinCapture {
    screen: (i32, i32),
}

impl WinCapture {
    pub fn new() -> Self {
        let screen = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let screen = (screen.0.max(1), screen.1.max(1));
        Self { screen }
    }
}

impl Default for WinCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl InputCapture for WinCapture {
    fn start(&mut self, tx: mpsc::UnboundedSender<Event>) -> anyhow::Result<()> {
        let _ = TX.set(tx);
        let (w, h) = self.screen;
        SCREEN_W.store(w, Ordering::SeqCst);
        SCREEN_H.store(h, Ordering::SeqCst);
        CENTER_X.store(w / 2, Ordering::SeqCst);
        CENTER_Y.store(h / 2, Ordering::SeqCst);
        // LL hook 與其 message pump 必須同 thread（callback 在裝 hook 的 thread context 跑）。
        thread::Builder::new()
            .name("quickvm-capture".into())
            .spawn(run_hooks)?;
        Ok(())
    }

    fn set_grab(&mut self, grab: bool) {
        GRAB.store(grab, Ordering::SeqCst);
        let (cx, cy) = (
            CENTER_X.load(Ordering::SeqCst),
            CENTER_Y.load(Ordering::SeqCst),
        );
        unsafe {
            // 進 / 出 grab 都先把游標釘回中心：進 grab 是相對位移的基準點，
            // 出 grab 避免游標停在 enter 邊緣反覆觸發。
            SetCursorPos(cx, cy);
            // 游標可見性 best-effort：ShowCursor 是 per-process refcount，從非 GUI thread
            // 呼叫未必生效；warp-to-center 已讓游標釘在中心不亂跑，隱藏只是 polish。
            if grab {
                while ShowCursor(0) >= 0 {}
            } else {
                while ShowCursor(1) < 0 {}
            }
        }
    }
}

fn run_hooks() {
    let _ = HOOK_THREAD_ID.set(unsafe { GetCurrentThreadId() });
    // hmod = null、dwThreadId = 0：LL hook 是全域的特例，callback 仍跑在本程序本 thread，
    // 不需 DLL 注入。失敗多半是極少數受限環境，記錄後讓 thread 結束（capture 不可用）。
    let kbd = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_proc), std::ptr::null_mut(), 0) };
    let mouse =
        unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), std::ptr::null_mut(), 0) };
    if kbd.is_null() || mouse.is_null() {
        tracing::error!("SetWindowsHookExW 失敗 —— 無法安裝低階 hook");
        return;
    }
    tracing::info!(
        w = SCREEN_W.load(Ordering::SeqCst),
        h = SCREEN_H.load(Ordering::SeqCst),
        "WH_*_LL hook 啟動，捕捉鍵鼠事件"
    );
    // LL hook 的 callback 由系統直接呼叫，不需 Translate/Dispatch；但 thread 必須有
    // message loop 才會收到回呼。GetMessage 回 0（WM_QUIT）/ -1（error）時退出。
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {}
}

static HOOK_THREAD_ID: OnceLock<u32> = OnceLock::new();

fn send(ev: Event) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(ev); // UnboundedSender::send 非阻塞 —— 滿足 LL hook 不可久佔的要求
    }
}

/// 鍵盤 LL hook：scancode(+extended) → HID usage，grab 時吞事件。
unsafe extern "system" fn kbd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // ncode < 0（非 HC_ACTION）依約定必須原樣轉交，不得處理。
    if ncode < 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam) };
    }
    let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    // 自己（或其他注入器）合成的事件不抓 —— 否則同機 capture+inject 會自噬成迴圈。
    if kb.flags & LLKHF_INJECTED != 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam) };
    }
    let dir = match wparam as u32 {
        WM_KEYDOWN | WM_SYSKEYDOWN => Direction::Press, // SYS variant = Alt 組合 / F10
        WM_KEYUP | WM_SYSKEYUP => Direction::Release,
        _ => return unsafe { CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam) },
    };
    let extended = kb.flags & LLKHF_EXTENDED != 0;
    if let Some(usage) = scancode_to_hid(kb.scanCode, extended) {
        send(Event::Key { usage, dir });
    }
    if GRAB.load(Ordering::SeqCst) {
        1 // 吞掉，不傳給本機 app
    } else {
        unsafe { CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam) }
    }
}

/// 滑鼠 LL hook：移動 / 按鈕 / 滾輪，grab 時 warp-to-center + 吞事件。
unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode < 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam) };
    }
    let ms = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
    if ms.flags & LLMHF_INJECTED != 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam) };
    }
    let g = GRAB.load(Ordering::SeqCst);
    let msg = wparam as u32;

    if msg == WM_MOUSEMOVE {
        let (cx, cy) = (
            CENTER_X.load(Ordering::SeqCst),
            CENTER_Y.load(Ordering::SeqCst),
        );
        if g {
            // warp 回中心的回響事件 —— 其反向 delta 會抵銷真實移動，過濾掉。
            if ms.pt.x == cx && ms.pt.y == cy {
                return 1;
            }
            let dx = (ms.pt.x - cx) as f64;
            let dy = (ms.pt.y - cy) as f64;
            if dx != 0.0 || dy != 0.0 {
                send(Event::MotionRel { dx, dy });
            }
            unsafe { SetCursorPos(cx, cy) };
            return 1;
        }
        // 非 grab：絕對座標正規化 [0,1]（主螢幕；多螢幕副螢幕座標會 clamp）。
        let w = SCREEN_W.load(Ordering::SeqCst) as f64;
        let h = SCREEN_H.load(Ordering::SeqCst) as f64;
        send(Event::MotionAbs {
            x: (ms.pt.x as f64 / w).clamp(0.0, 1.0),
            y: (ms.pt.y as f64 / h).clamp(0.0, 1.0),
        });
        return unsafe { CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam) };
    }

    if let Some(ev) = translate_mouse(msg, ms) {
        send(ev);
    }
    if g {
        1
    } else {
        unsafe { CallNextHookEx(std::ptr::null_mut(), ncode, wparam, lparam) }
    }
}

fn translate_mouse(msg: u32, ms: &MSLLHOOKSTRUCT) -> Option<Event> {
    let btn = |button, dir| Some(Event::Button { button, dir });
    match msg {
        WM_LBUTTONDOWN => btn(MouseButton::Left, Direction::Press),
        WM_LBUTTONUP => btn(MouseButton::Left, Direction::Release),
        WM_RBUTTONDOWN => btn(MouseButton::Right, Direction::Press),
        WM_RBUTTONUP => btn(MouseButton::Right, Direction::Release),
        WM_MBUTTONDOWN => btn(MouseButton::Middle, Direction::Press),
        WM_MBUTTONUP => btn(MouseButton::Middle, Direction::Release),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            // 哪顆 X 鍵在 mouseData 高位字（XBUTTON1 = Back、XBUTTON2 = Forward）。
            let hb = (ms.mouseData >> 16) as u16;
            let button = match hb {
                XBUTTON1 => MouseButton::Back,
                XBUTTON2 => MouseButton::Forward,
                _ => return None,
            };
            let dir = if msg == WM_XBUTTONDOWN {
                Direction::Press
            } else {
                Direction::Release
            };
            Some(Event::Button { button, dir })
        }
        // 滾輪 delta 在高位字（有號）；WHEEL_DELTA = 120 = 一個 tick，與本專案 Scroll 單位一致。
        // 正值：垂直 = 向上（遠離使用者）、水平 = 向右，對齊 macOS 後端慣例。
        WM_MOUSEWHEEL => {
            let d = (ms.mouseData >> 16) as i16 as i32;
            Some(Event::Scroll { dx: 0, dy: d })
        }
        WM_MOUSEHWHEEL => {
            let d = (ms.mouseData >> 16) as i16 as i32;
            Some(Event::Scroll { dx: d, dy: 0 })
        }
        _ => None,
    }
}

/// Windows set-1 scan code（`KBDLLHOOKSTRUCT.scanCode` + `LLKHF_EXTENDED`）
/// → USB HID Usage ID（Keyboard/Keypad Page 0x07）。
/// 對照：Microsoft "USB HID to PS/2 Scan Code Translation Table"；怪點對齊
/// GLFW src/win32_{window,init}.c 與 Chromium dom_code_data.inc（win 欄）。
///
/// `extended` = `flags & LLKHF_EXTENDED`（scan code 帶 E0 prefix）。本表吃的是
/// 「LL hook 看到的值」，不是鍵盤硬體值 —— Windows 驅動層有四個著名怪點：
/// - **Pause**：硬體 make 是 E1 1D 45，但 LL hook 報 scan=0x45 **不帶** extended。
/// - **NumLock**：硬體是 0x45 不帶 prefix，但 LL hook 報 scan=0x45 **帶** extended
///   —— 跟 Pause 恰好互換，照報的值映射即可。
/// - **Ctrl+Pause(Break)**：報 E0 46，normalize 回 Pause。
/// - **Alt+PrintScreen(SysRq)**：報 0x54，normalize 回 PrintScreen。
fn scancode_to_hid(scan: u32, extended: bool) -> Option<u16> {
    if extended {
        // E0 系列：右側修飾鍵、nav 六鍵、方向鍵、小鍵盤 Enter / 除號。
        // 同 scan 帶不帶 E0 是兩顆不同實體鍵，故 extended 必須分支。
        return Some(match scan {
            0x1C => 0x58, // Keypad Enter（與主 Enter 同 scan，靠 E0 區分）
            0x1D => 0xE4, // RightControl
            0x35 => 0x54, // Keypad /
            0x37 => 0x46, // PrintScreen（硬體 E0 2A E0 37，LL hook 只報 E0 37）
            0x38 => 0xE6, // RightAlt（AltGr 會先送一顆 LCtrl 再送這個；兩顆都照轉）
            0x45 => 0x53, // NumLock —— 怪點：Windows 報帶 E0（與 Pause 互換）
            0x46 => 0x48, // Ctrl+Pause(Break) 報 E0 46 → normalize 回 Pause
            0x47 => 0x4A, // Home
            0x48 => 0x52, // UpArrow
            0x49 => 0x4B, // PageUp
            0x4B => 0x50, // LeftArrow
            0x4D => 0x4F, // RightArrow
            0x4F => 0x4D, // End
            0x50 => 0x51, // DownArrow
            0x51 => 0x4E, // PageDown
            0x52 => 0x49, // Insert
            0x53 => 0x4C, // Delete（forward delete）
            0x5B => 0xE3, // LeftGUI / Win
            0x5C => 0xE7, // RightGUI / Win
            0x5D => 0x65, // Menu / Application
            0x36 => 0xE5, // RShift：CJK IME 會替它設 extended（GLFW 同款 hack），仍視為 RShift
            // fake LShift（鍵盤替 nav/方向鍵插入的影子 Shift，E0 2A / E0 AA）：
            // 轉發出去會在對端產生幽靈 Shift，丟棄。
            0x2A => return None,
            _ => return None,
        });
    }
    Some(match scan {
        0x1E => 0x04, // A
        0x30 => 0x05, // B
        0x2E => 0x06, // C
        0x20 => 0x07, // D
        0x12 => 0x08, // E
        0x21 => 0x09, // F
        0x22 => 0x0A, // G
        0x23 => 0x0B, // H
        0x17 => 0x0C, // I
        0x24 => 0x0D, // J
        0x25 => 0x0E, // K
        0x26 => 0x0F, // L
        0x32 => 0x10, // M
        0x31 => 0x11, // N
        0x18 => 0x12, // O
        0x19 => 0x13, // P
        0x10 => 0x14, // Q
        0x13 => 0x15, // R
        0x1F => 0x16, // S
        0x14 => 0x17, // T
        0x16 => 0x18, // U
        0x2F => 0x19, // V
        0x11 => 0x1A, // W
        0x2D => 0x1B, // X
        0x15 => 0x1C, // Y
        0x2C => 0x1D, // Z
        0x02 => 0x1E, // 1
        0x03 => 0x1F, // 2
        0x04 => 0x20, // 3
        0x05 => 0x21, // 4
        0x06 => 0x22, // 5
        0x07 => 0x23, // 6
        0x08 => 0x24, // 7
        0x09 => 0x25, // 8
        0x0A => 0x26, // 9
        0x0B => 0x27, // 0
        0x1C => 0x28, // Enter（主鍵盤；小鍵盤 Enter 是 E0 1C 走 extended）
        0x01 => 0x29, // Escape
        0x0E => 0x2A, // Backspace
        0x0F => 0x2B, // Tab
        0x39 => 0x2C, // Space
        0x0C => 0x2D, // - _
        0x0D => 0x2E, // = +
        0x1A => 0x2F, // [ {
        0x1B => 0x30, // ] }
        0x2B => 0x31, // \ |（US；ISO # ~ 同 scan，HID 本應 0x32，收斂到 0x31）
        0x27 => 0x33, // ; :
        0x28 => 0x34, // ' "
        0x29 => 0x35, // ` ~
        0x33 => 0x36, // , <
        0x34 => 0x37, // . >
        0x35 => 0x38, // / ?
        0x3A => 0x39, // CapsLock
        0x3B => 0x3A, // F1
        0x3C => 0x3B, // F2
        0x3D => 0x3C, // F3
        0x3E => 0x3D, // F4
        0x3F => 0x3E, // F5
        0x40 => 0x3F, // F6
        0x41 => 0x40, // F7
        0x42 => 0x41, // F8
        0x43 => 0x42, // F9
        0x44 => 0x43, // F10
        0x57 => 0x44, // F11（AT 鍵盤後加，scan 不連續）
        0x58 => 0x45, // F12
        0x54 => 0x46, // Alt+PrintScreen 報 SysRq 0x54 → normalize 回 PrintScreen
        0x46 => 0x47, // ScrollLock
        0x45 => 0x48, // Pause —— 怪點：LL hook 報 0x45 不帶 extended
        0x47 => 0x5F, // Keypad 7
        0x48 => 0x60, // Keypad 8
        0x49 => 0x61, // Keypad 9
        0x4B => 0x5C, // Keypad 4
        0x4C => 0x5D, // Keypad 5
        0x4D => 0x5E, // Keypad 6
        0x4F => 0x59, // Keypad 1
        0x50 => 0x5A, // Keypad 2
        0x51 => 0x5B, // Keypad 3
        0x52 => 0x62, // Keypad 0
        0x53 => 0x63, // Keypad .
        0x37 => 0x55, // Keypad *
        0x4A => 0x56, // Keypad -
        0x4E => 0x57, // Keypad +
        0x56 => 0x64, // 非 US \ |（ISO 102 鍵多的那顆）
        0x1D => 0xE0, // LeftControl
        0x2A => 0xE1, // LeftShift
        0x36 => 0xE5, // RightShift —— 不帶 extended（跟 RCtrl/RAlt 不同）
        0x38 => 0xE2, // LeftAlt
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancode_letters_and_extended_disambiguation() {
        assert_eq!(scancode_to_hid(0x1E, false), Some(0x04)); // A
        // 同 scan 0x1C：主 Enter vs 小鍵盤 Enter，靠 extended 區分成兩顆不同 HID。
        assert_eq!(scancode_to_hid(0x1C, false), Some(0x28)); // Enter
        assert_eq!(scancode_to_hid(0x1C, true), Some(0x58)); // Keypad Enter
        // 右側修飾鍵帶 extended，左側不帶。
        assert_eq!(scancode_to_hid(0x1D, false), Some(0xE0)); // LeftControl
        assert_eq!(scancode_to_hid(0x1D, true), Some(0xE4)); // RightControl
        // RightShift 是不帶 extended 的特例。
        assert_eq!(scancode_to_hid(0x36, false), Some(0xE5));
        // 未知 scan 回 None。
        assert_eq!(scancode_to_hid(0xFE, false), None);
    }

    #[test]
    fn fake_lshift_dropped() {
        // 鍵盤替 nav 鍵插入的影子 LShift（E0 2A）丟棄，避免對端幽靈 Shift。
        assert_eq!(scancode_to_hid(0x2A, true), None);
    }
}
