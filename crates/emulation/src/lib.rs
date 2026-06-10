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
        use enigo::{Keyboard, Mouse};
        match ev {
            Event::MotionAbs { x, y } => {
                let px = (x * self.screen.0 as f64).round() as i32;
                let py = (y * self.screen.1 as f64).round() as i32;
                self.enigo.move_mouse(px, py, enigo::Coordinate::Abs)?;
            }
            Event::MotionRel { dx, dy } => {
                self.enigo
                    .move_mouse(*dx as i32, *dy as i32, enigo::Coordinate::Rel)?;
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
            Event::Key { usage, dir } => match hid_to_enigo_key(*usage) {
                Some(key) => self.enigo.key(key, map_dir(*dir))?,
                None => tracing::warn!(usage, "未映射的 HID usage，略過"),
            },
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

/// USB HID Usage ID（Keyboard/Keypad Page 0x07）→ enigo 0.6 `Key`。
/// 字母/數字/符號走 `Key::Unicode`（base glyph，shift 由修飾鍵 usage 另外帶）。
/// macOS 限制：enigo 無左右獨立 Alt/Meta，0xE2/0xE6 收斂為 Alt、0xE3/0xE7 收斂為 Meta。
fn hid_to_enigo_key(usage: u16) -> Option<enigo::Key> {
    use enigo::Key;
    Some(match usage {
        0x04 => Key::Unicode('a'),
        0x05 => Key::Unicode('b'),
        0x06 => Key::Unicode('c'),
        0x07 => Key::Unicode('d'),
        0x08 => Key::Unicode('e'),
        0x09 => Key::Unicode('f'),
        0x0A => Key::Unicode('g'),
        0x0B => Key::Unicode('h'),
        0x0C => Key::Unicode('i'),
        0x0D => Key::Unicode('j'),
        0x0E => Key::Unicode('k'),
        0x0F => Key::Unicode('l'),
        0x10 => Key::Unicode('m'),
        0x11 => Key::Unicode('n'),
        0x12 => Key::Unicode('o'),
        0x13 => Key::Unicode('p'),
        0x14 => Key::Unicode('q'),
        0x15 => Key::Unicode('r'),
        0x16 => Key::Unicode('s'),
        0x17 => Key::Unicode('t'),
        0x18 => Key::Unicode('u'),
        0x19 => Key::Unicode('v'),
        0x1A => Key::Unicode('w'),
        0x1B => Key::Unicode('x'),
        0x1C => Key::Unicode('y'),
        0x1D => Key::Unicode('z'),
        0x1E => Key::Unicode('1'),
        0x1F => Key::Unicode('2'),
        0x20 => Key::Unicode('3'),
        0x21 => Key::Unicode('4'),
        0x22 => Key::Unicode('5'),
        0x23 => Key::Unicode('6'),
        0x24 => Key::Unicode('7'),
        0x25 => Key::Unicode('8'),
        0x26 => Key::Unicode('9'),
        0x27 => Key::Unicode('0'),
        0x2D => Key::Unicode('-'),
        0x2E => Key::Unicode('='),
        0x2F => Key::Unicode('['),
        0x30 => Key::Unicode(']'),
        0x31 => Key::Unicode('\\'),
        0x33 => Key::Unicode(';'),
        0x34 => Key::Unicode('\''),
        0x35 => Key::Unicode('`'),
        0x36 => Key::Unicode(','),
        0x37 => Key::Unicode('.'),
        0x38 => Key::Unicode('/'),
        0x28 => Key::Return,
        0x29 => Key::Escape,
        0x2A => Key::Backspace,
        0x2B => Key::Tab,
        0x2C => Key::Space,
        0x4C => Key::Delete,
        0x39 => Key::CapsLock,
        0x3A => Key::F1,
        0x3B => Key::F2,
        0x3C => Key::F3,
        0x3D => Key::F4,
        0x3E => Key::F5,
        0x3F => Key::F6,
        0x40 => Key::F7,
        0x41 => Key::F8,
        0x42 => Key::F9,
        0x43 => Key::F10,
        0x44 => Key::F11,
        0x45 => Key::F12,
        0x49 => Key::Help, // HID Insert；enigo Insert 在 macOS 不可用 → Help
        0x4A => Key::Home,
        0x4B => Key::PageUp,
        0x4D => Key::End,
        0x4E => Key::PageDown,
        0x4F => Key::RightArrow,
        0x50 => Key::LeftArrow,
        0x51 => Key::DownArrow,
        0x52 => Key::UpArrow,
        0xE0 => Key::LControl,
        0xE1 => Key::LShift,
        0xE2 => Key::Alt,
        0xE3 => Key::Meta,
        0xE4 => Key::RControl,
        0xE5 => Key::RShift,
        0xE6 => Key::Alt,
        0xE7 => Key::Meta,
        _ => return None,
    })
}
