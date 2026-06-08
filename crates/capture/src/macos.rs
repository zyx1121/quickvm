//! macOS CGEventTap 捕捉後端。
//!
//! 在獨立 thread 跑 CFRunLoop；callback 把 CGEvent 轉成 quickvm Event 送進 channel，
//! grab=true 時 `Drop`（吞掉本機輸入），否則 `Keep`（放行）。
//!
//! TODO: 監聽 TapDisabledByTimeout/UserInput 後 `tap.enable()` re-enable
//!       （目前 callback 極快不會 timeout，先略；production 必補）。
//! TODO: KeyDown/KeyUp 的 CGKeyCode → HID usage 映射（先只做滑鼠）。
//! 限制：Secure Input（密碼欄）下 event tap 收不到鍵盤事件，macOS 設計使然。

use crate::InputCapture;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};
use core_graphics::event::CGEvent;
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
    tracing::info!(w, h, "CGEventTap 啟動，捕捉滑鼠事件");
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
        _ => None,
    }
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
