//! 本機剪貼簿讀寫（arboard）+ 去重指紋。
//!
//! 讀寫都包 `spawn_blocking`：arboard 是同步 API，Windows 上 `OpenClipboard`
//! 可能被其他 app 短暫佔住，不能卡住 async event loop。
//! 任何失敗（空 / 非文字 / 被佔用 / 超上限）都靜默降級 —— 剪貼簿是附加功能，
//! 絕不影響 KVM 主流程。

use quickvm_proto::CLIPBOARD_MAX;
use std::hash::{DefaultHasher, Hash, Hasher};

/// 內容指紋，去重用（只在 process 內比對，兩端各自記各自的）。
/// 同步過的內容不重送 / 不重寫 —— 否則每次切換都覆寫對端剪貼簿（擾動 clipboard
/// history），回送再觸發回聲。
pub fn fingerprint(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// 讀本機剪貼簿文字。空 / 非文字 / 超過 `CLIPBOARD_MAX` / 讀取失敗都回 `None`。
pub async fn read_text() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let text = arboard::Clipboard::new().ok()?.get_text().ok()?;
        if text.is_empty() {
            return None;
        }
        if text.len() > CLIPBOARD_MAX {
            tracing::warn!(
                len = text.len(),
                max = CLIPBOARD_MAX,
                "剪貼簿超過上限，跳過同步"
            );
            return None;
        }
        Some(text)
    })
    .await
    .ok()
    .flatten()
}

/// 寫本機剪貼簿。失敗只 log。
pub async fn write_text(text: String) {
    let _ = tokio::task::spawn_blocking(move || {
        if let Err(e) = arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
            tracing::warn!(error = %e, "剪貼簿寫入失敗");
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_dedups() {
        assert_eq!(fingerprint("hello"), fingerprint("hello"));
        assert_ne!(fingerprint("hello"), fingerprint("hello "));
        assert_eq!(fingerprint("中文 emoji 🦀"), fingerprint("中文 emoji 🦀"));
    }
}
