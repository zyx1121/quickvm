//! 線協定：可靠訊息（鍵盤 / 按鈕 / 捲動 / 控制）走 QUIC reliable stream，
//! 指標移動走 unreliable datagram（帶序號，接收端丟棄過期 / 亂序封包）。

use quickvm_event::{Control, Event};
use serde::{Deserialize, Serialize};

/// 剪貼簿文字上限。超過不送（不截斷 —— 截斷的半截內容貼出去比沒同步更糟）。
pub const CLIPBOARD_MAX: usize = 1 << 20; // 1 MiB

/// 剪貼簿檔案總量上限。檔案內容走 streaming（不全載記憶體），上限擋的是
/// 「同步一次佔線太久 + 對端暫存盤爆」，不是 frame 大小。
pub const FILES_MAX_TOTAL: u64 = 256 << 20; // 256 MiB

/// uni stream 第一個 byte：頻道 tag。0x00 = 單則 postcard `Reliable`（小訊息），
/// 0x01 = 檔案傳輸（`FilesHeader` + 各檔 raw bytes 串接，見 transport）。
pub const STREAM_TAG_MSG: u8 = 0x00;
pub const STREAM_TAG_FILES: u8 = 0x01;

/// 檔案傳輸 stream 的 header（tag byte 之後、內容串流之前）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilesHeader {
    pub files: Vec<FileMeta>,
}

/// 單一檔案的傳輸 metadata。`name` 只允許檔名本體（接收端仍會 sanitize，
/// 防 path traversal）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileMeta {
    pub name: String,
    pub size: u64,
}

pub fn encode_files_header(h: &FilesHeader) -> Vec<u8> {
    postcard::to_allocvec(h).expect("encode files header")
}

pub fn decode_files_header(buf: &[u8]) -> anyhow::Result<FilesHeader> {
    Ok(postcard::from_bytes(buf)?)
}

/// 走 reliable stream 的訊息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Reliable {
    /// 鍵盤 / 按鈕 / 捲動（不含 `MotionAbs`）。
    Input(Event),
    Control(Control),
    /// 剪貼簿同步（純文字）。雙向：主控端 Enter 時推本機剪貼簿，被控端 Leave 時回推。
    Clipboard(String),
}

/// 走 datagram 的指標移動，帶序號供接收端丟棄過期封包。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Motion {
    pub seq: u64,
    pub x: f64,
    pub y: f64,
}

pub fn encode_reliable(msg: &Reliable) -> Vec<u8> {
    postcard::to_allocvec(msg).expect("encode reliable")
}

pub fn decode_reliable(buf: &[u8]) -> anyhow::Result<Reliable> {
    Ok(postcard::from_bytes(buf)?)
}

pub fn encode_motion(m: &Motion) -> Vec<u8> {
    postcard::to_allocvec(m).expect("encode motion")
}

pub fn decode_motion(buf: &[u8]) -> anyhow::Result<Motion> {
    Ok(postcard::from_bytes(buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_roundtrip() {
        let msg = Reliable::Clipboard("hello 中文 🦀\r\nline2".to_string());
        let buf = encode_reliable(&msg);
        assert_eq!(decode_reliable(&buf).unwrap(), msg);
    }

    #[test]
    fn clipboard_max_fits_wire() {
        // 上限大小的內容必須編得進 transport 的 frame 上限（CLIPBOARD_MAX + 64）。
        let msg = Reliable::Clipboard("x".repeat(CLIPBOARD_MAX));
        assert!(encode_reliable(&msg).len() <= CLIPBOARD_MAX + 64);
    }

    #[test]
    fn input_roundtrip_unchanged() {
        // 加 variant 後既有訊息編解碼不變。
        let msg = Reliable::Control(Control::Leave);
        let buf = encode_reliable(&msg);
        assert_eq!(decode_reliable(&buf).unwrap(), msg);
    }
}
