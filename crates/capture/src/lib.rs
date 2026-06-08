//! 輸入捕捉抽象。每平台一個後端：捕捉實體鍵鼠，並在「控制對端」時吞掉本機事件。
//!
//! v1 只提供 `ManualCapture`（不接 OS API），讓鏈路先跑通。真實後端：
//! - TODO(v1.1 macOS): `CGEventTap`（`kCGHeadInsertEventTap` + callback `return NULL` 吞事件；
//!   需 Accessibility 權限；監聽 `kCGEventTapDisabledByTimeout` 自動 re-enable）。
//! - TODO(v1.1 Windows): `WH_KEYBOARD_LL` / `WH_MOUSE_LL`（callback `return 1` 吞事件；
//!   callback 須在 1 秒內 return，重活丟 worker thread）。

use quickvm_event::Event;
use tokio::sync::mpsc;

/// 捕捉後端。
pub trait InputCapture: Send {
    /// 啟動捕捉，事件送進 channel。
    fn start(&mut self, tx: mpsc::UnboundedSender<Event>) -> anyhow::Result<()>;
    /// 是否吞掉本機事件 —— 控制對端時 `true`，本機自用時 `false`。
    fn set_grab(&mut self, grab: bool);
}

/// 佔位後端：不接 OS API，由呼叫端 `feed()` 手動注入事件（開發 / 測試用）。
#[derive(Default)]
pub struct ManualCapture {
    tx: Option<mpsc::UnboundedSender<Event>>,
    grab: bool,
}

impl ManualCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// 手動注入一個「被捕捉到」的事件，暫代真實 OS 捕捉。
    pub fn feed(&self, ev: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
    }
}

impl InputCapture for ManualCapture {
    fn start(&mut self, tx: mpsc::UnboundedSender<Event>) -> anyhow::Result<()> {
        self.tx = Some(tx);
        tracing::info!("ManualCapture started (stub — TODO: 平台 event tap / hook)");
        Ok(())
    }

    fn set_grab(&mut self, grab: bool) {
        self.grab = grab;
        tracing::debug!(grab, "capture grab toggled");
    }
}
