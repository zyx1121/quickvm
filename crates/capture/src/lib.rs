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

/// 捕捉後端。
pub trait InputCapture: Send {
    /// 啟動捕捉，事件送進 channel。
    fn start(&mut self, tx: mpsc::UnboundedSender<Event>) -> anyhow::Result<()>;
    /// 是否吞掉本機事件 —— 控制對端時 `true`，本機自用時 `false`。
    fn set_grab(&mut self, grab: bool);
}

/// 當前平台的捕捉後端：macOS = CGEventTap，其他 = ManualCapture stub。
pub fn default_capture() -> Box<dyn InputCapture> {
    #[cfg(target_os = "macos")]
    {
        Box::new(MacCapture::new())
    }
    #[cfg(not(target_os = "macos"))]
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
