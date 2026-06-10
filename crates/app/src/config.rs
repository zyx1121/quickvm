//! 設定檔：兩台螢幕的相對位置 + 尺寸 + 被控端位址。
//! 決定哪個邊緣能「穿過去」到對端、進入後游標落在對端哪一側。

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

/// 對端（remote）相對本機（local）的方位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// 一台螢幕的像素尺寸。
/// 目前邊緣偵測走正規化座標不需本機尺寸；保留 `local` 給後續「等比映射」
/// （Mac 直立 vs Windows 橫向長寬比不同，位置對但非等比）與游標歸位用。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Screen {
    pub width: f64,
    pub height: f64,
}

/// 對端螢幕：尺寸 + 它在本機的哪一側。
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Remote {
    pub width: f64,
    pub height: f64,
    pub side: Side,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// 被控端位址，如 "192.168.1.120:7777"。
    pub server: String,
    /// 本機螢幕（Mac）。保留給等比映射 / 游標歸位（見 `Screen`）。
    #[allow(dead_code)]
    pub local: Screen,
    /// 對端螢幕（Windows）。
    pub remote: Remote,
}

impl Config {
    /// 從 path（或預設路徑）讀取並解析。
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let path = path.unwrap_or_else(default_path);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("讀設定檔失敗：{}（可複製 config.example.toml）", path.display()))?;
        toml::from_str(&text).with_context(|| format!("解析設定檔失敗：{}", path.display()))
    }
}

/// 預設設定檔位置：`~/.config/quickvm/config.toml`。
pub fn default_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".config/quickvm/config.toml"))
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}
