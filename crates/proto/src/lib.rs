//! 線協定：可靠訊息（鍵盤 / 按鈕 / 捲動 / 控制）走 QUIC reliable stream，
//! 指標移動走 unreliable datagram（帶序號，接收端丟棄過期 / 亂序封包）。

use quickvm_event::{Control, Event};
use serde::{Deserialize, Serialize};

/// 走 reliable stream 的訊息。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Reliable {
    /// 鍵盤 / 按鈕 / 捲動（不含 `MotionAbs`）。
    Input(Event),
    Control(Control),
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
