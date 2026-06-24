//! 輸入捕捉抽象 + 平台後端。
//! macOS = CGEventTap（見 `macos.rs`）；其他平台暫用 ManualCapture stub。
//!
//! TODO(v1.1 Windows): `WH_KEYBOARD_LL` / `WH_MOUSE_LL`（callback return 1 吞事件，
//!   callback 須 1 秒內 return）。

use quickvm_event::Event;
use tokio::sync::mpsc;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacCapture;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::WinCapture;

/// 捕捉後端。
pub trait InputCapture: Send {
    /// 啟動捕捉，事件送進 channel。
    fn start(&mut self, tx: mpsc::UnboundedSender<Event>) -> anyhow::Result<()>;
    /// 是否吞掉本機事件 —— 控制對端時 `true`，本機自用時 `false`。
    fn set_grab(&mut self, grab: bool);
    /// 主控端本機的「逃生」請求：grab 期間使用者按下平台 panic 組合（macOS = 左右 Cmd 同時按）
    /// 時，後端已**就地**解除 grab（完全不碰網路，網路死了照樣搶回本機）；app 取走此旗標把
    /// 狀態機交還本機並通知對端 release 黏鍵。無 panic 組合的平台用預設 `false`。
    fn escape_requested(&self) -> bool {
        false
    }
}

/// 當前平台的捕捉後端：macOS = CGEventTap、Windows = WH_*_LL hook、其他 = ManualCapture stub。
pub fn default_capture() -> Box<dyn InputCapture> {
    #[cfg(target_os = "macos")]
    {
        Box::new(MacCapture::new())
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(WinCapture::new())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Box::new(ManualCapture::new())
    }
}

/// 佔位後端：不接 OS API，由呼叫端 `feed()` 手動注入事件（非 macOS / 測試用）。
#[derive(Default)]
pub struct ManualCapture {
    tx: Option<mpsc::UnboundedSender<Event>>,
    grab: bool,
}

impl ManualCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// 手動注入一個「被捕捉到」的事件。
    #[allow(dead_code)]
    pub fn feed(&self, ev: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
    }
}

impl InputCapture for ManualCapture {
    fn start(&mut self, tx: mpsc::UnboundedSender<Event>) -> anyhow::Result<()> {
        self.tx = Some(tx);
        let _ = self.grab;
        tracing::info!("ManualCapture started (stub)");
        Ok(())
    }

    fn set_grab(&mut self, grab: bool) {
        self.grab = grab;
    }
}
