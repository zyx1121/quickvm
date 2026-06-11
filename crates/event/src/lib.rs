//! 平台無關的輸入事件模型。
//! 鍵碼以 USB HID Usage ID 為錨（跨平台對稱），不傳 OS keycode 或字元。

use serde::{Deserialize, Serialize};

/// 按鍵 / 按鈕方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Press,
    Release,
}

/// 滑鼠鍵。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// 修飾鍵狀態（bit flags，對應實體修飾鍵，與佈局無關）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Modifiers(pub u16);

impl Modifiers {
    pub const CTRL: u16 = 1 << 0;
    pub const SHIFT: u16 = 1 << 1;
    pub const ALT: u16 = 1 << 2; // mac: Option
    pub const META: u16 = 1 << 3; // mac: Command / win: Win
}

/// 一個輸入事件。
///
/// 傳輸分流（見 `quickvm-proto`）：
/// - `MotionAbs` 走 **unreliable datagram**（高頻、丟了下一筆自癒）。
/// - 其餘走 **reliable stream**（漏 keyup = 卡鍵；捲動累加不能丟）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// 鍵盤事件。`usage` = USB HID Usage ID（Keyboard/Keypad page 0x07）。
    Key { usage: u16, dir: Direction },
    /// 滑鼠鍵。
    Button { button: MouseButton, dir: Direction },
    /// 指標絕對位置，正規化到 [0.0, 1.0]（跨解析度 / DPI 無關）。
    MotionAbs { x: f64, y: f64 },
    /// 指標相對位移（點）。grab 模式下系統游標 freeze，絕對位置失效，
    /// 改累積 delta 算虛擬游標位置。只在主控端本機 capture→app 流動，不上線。
    MotionRel { dx: f64, dy: f64 },
    /// 高解析度捲動（120 = 一個滾輪 tick）。
    Scroll { dx: i32, dy: i32 },
}

/// 螢幕切換 / 連線控制訊息（永遠走 reliable stream）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Control {
    /// 游標進入對端螢幕：帶進入點（正規化）+ 當前修飾鍵狀態（讓對端對齊，避免黏鍵）。
    Enter { x: f64, y: f64, mods: Modifiers },
    /// 游標離開對端，控制權交還主控端。
    Leave,
}
