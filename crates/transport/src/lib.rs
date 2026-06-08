//! QUIC 傳輸（骨架）。鍵盤 / 控制走 reliable uni stream、指標走 unreliable datagram。
//!
//! TODO(下一步，用 context7 查 quinn / rcgen / rustls 精確 API 後填實作)：
//!   - `serve`: rcgen 自簽 + `ServerConfig::with_single_cert` + 允許 datagram + accept 迴圈。
//!   - `connect`: skip-verify client。TODO(security): 改 SSH-like fingerprint 信任，別長期 skip。
//!   - `Link::send_reliable`: `open_uni` + length-delimited bincode frame。
//!   - `Link::send_motion`: `send_datagram`。
//!   - 收事件迴圈：`accept_uni` 讀可靠、`read_datagram` 讀指標、依 seq 丟過期封包。
//!   - CC: 換 BBR / 調大 initial cwnd，避免 loss-based CC 在 WiFi 丟包時收縮、反增延遲。

use anyhow::Result;
use quickvm_proto::{Motion, Reliable};
use std::net::SocketAddr;

/// 一條連線，封裝「送可靠訊息」與「送指標 datagram」。
pub struct Link {
    motion_seq: u64,
}

impl Link {
    /// 送可靠訊息（鍵盤 / 按鈕 / 捲動 / 控制）。
    pub async fn send_reliable(&self, msg: &Reliable) -> Result<()> {
        let _ = msg;
        anyhow::bail!("transport::Link::send_reliable 尚未實作 (TODO: quinn open_uni)")
    }

    /// 送指標移動（datagram，帶遞增 seq）。
    pub fn send_motion(&mut self, x: f64, y: f64) -> Result<()> {
        self.motion_seq += 1;
        let _ = Motion { seq: self.motion_seq, x, y };
        anyhow::bail!("transport::Link::send_motion 尚未實作 (TODO: quinn send_datagram)")
    }
}

/// 被控端：監聽並接受連線。
pub async fn serve(bind: SocketAddr) -> Result<()> {
    let _ = bind;
    anyhow::bail!("transport::serve 尚未實作 (TODO: quinn server endpoint + rcgen 自簽)")
}

/// 主控端：連到被控端。
pub async fn connect(addr: SocketAddr) -> Result<Link> {
    let _ = addr;
    anyhow::bail!("transport::connect 尚未實作 (TODO: quinn client connect + skip-verify)")
}
