//! 輸入注入抽象。v1 用 enigo（合成事件）；
//! TODO(v2): virtual-HID 後端（mac=Karabiner daemon、win=FakerInput）以繞 UAC / Secure Input。

use quickvm_event::{Direction, Event, MouseButton};

/// 注入後端：把事件重放到本機。
pub trait InputEmulator: Send {
    fn emit(&mut self, ev: &Event) -> anyhow::Result<()>;
}

/// enigo 後端（跨平台合成；控不了 elevated 視窗 / 密碼欄 —— 見 v2 virtual-HID）。
pub struct EnigoEmulator {
    enigo: enigo::Enigo,
    /// 主螢幕像素尺寸，用來把正規化座標還原成像素。
    screen: (i32, i32),
}

impl EnigoEmulator {
    pub fn new() -> anyhow::Result<Self> {
        use enigo::Mouse;
        let enigo = enigo::Enigo::new(&enigo::Settings::default())?;
        let screen = enigo.main_display().unwrap_or((1920, 1080));
        Ok(Self { enigo, screen })
    }
}

impl InputEmulator for EnigoEmulator {
    fn emit(&mut self, ev: &Event) -> anyhow::Result<()> {
        use enigo::Mouse;
        match ev {
            Event::MotionAbs { x, y } => {
                let px = (x * self.screen.0 as f64).round() as i32;
                let py = (y * self.screen.1 as f64).round() as i32;
                self.enigo.move_mouse(px, py, enigo::Coordinate::Abs)?;
            }
            Event::Button { button, dir } => {
                self.enigo.button(map_button(*button), map_dir(*dir))?;
            }
            Event::Scroll { dx, dy } => {
                if *dy != 0 {
                    self.enigo.scroll(*dy / 120, enigo::Axis::Vertical)?;
                }
                if *dx != 0 {
                    self.enigo.scroll(*dx / 120, enigo::Axis::Horizontal)?;
                }
            }
            Event::Key { usage, dir } => {
                // TODO(v1.1): HID usage -> enigo::Key 映射表。enigo 吃 OS keycode / Unicode，
                //   需建「HID usage ↔ 平台 keycode」對照（Windows scancode / macOS CGKeyCode）。
                let _ = dir;
                tracing::warn!(usage, "key emit 尚未映射 (TODO: HID usage table)");
            }
        }
        Ok(())
    }
}

fn map_button(b: MouseButton) -> enigo::Button {
    match b {
        MouseButton::Left => enigo::Button::Left,
        MouseButton::Right => enigo::Button::Right,
        MouseButton::Middle => enigo::Button::Middle,
        MouseButton::Back => enigo::Button::Back,
        MouseButton::Forward => enigo::Button::Forward,
    }
}

fn map_dir(d: Direction) -> enigo::Direction {
    match d {
        Direction::Press => enigo::Direction::Press,
        Direction::Release => enigo::Direction::Release,
    }
}
