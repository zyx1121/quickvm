//! 檔案剪貼簿（file list）讀寫 —— arboard 只支援 text / image，檔案列表要平台 API：
//! macOS = NSPasteboard file URLs、Windows = CF_HDROP。
//!
//! 「複製檔案」在系統剪貼簿裡只是**路徑參照**，跨機同步路徑無意義 —— 傳輸層
//! 把內容串流過去、對端落地暫存再把剪貼簿指過去（見 transport `send_files_on`）。
//! 與文字同款原則：任何失敗靜默降級，絕不影響 KVM 主流程。

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

/// 檔案批次指紋：路徑 + size + mtime（讀整檔內容 hash 太貴）。
/// 前綴 0x02 跟文字指紋（0x01）分域，同一個 `last_synced` 槽不互撞。
pub fn fingerprint(paths: &[PathBuf]) -> u64 {
    let mut h = DefaultHasher::new();
    2u8.hash(&mut h);
    for p in paths {
        p.hash(&mut h);
        if let Ok(md) = std::fs::metadata(p) {
            md.len().hash(&mut h);
            if let Ok(t) = md.modified() {
                t.hash(&mut h);
            }
        }
    }
    h.finish()
}

/// 文字指紋搬進同一個分域系統（0x01 前綴），取代 clipboard.rs 的舊版。
pub fn fingerprint_text(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    1u8.hash(&mut h);
    text.hash(&mut h);
    h.finish()
}

/// 讀本機剪貼簿的檔案列表。沒有檔案 / 失敗回 `None`；
/// 過濾掉資料夾與消失的路徑（v1 只支援一般檔案，同 Deskflow）。
pub async fn read_files() -> Option<Vec<PathBuf>> {
    tokio::task::spawn_blocking(|| {
        let paths = platform::read_file_list()?;
        let files: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_file()).collect();
        (!files.is_empty()).then_some(files)
    })
    .await
    .ok()
    .flatten()
}

/// 把（已落地的）檔案列表寫進本機剪貼簿。失敗只 log。
pub async fn write_files(paths: Vec<PathBuf>) {
    let _ = tokio::task::spawn_blocking(move || {
        if let Err(e) = platform::write_file_list(&paths) {
            tracing::warn!(error = %e, "檔案剪貼簿寫入失敗");
        }
    })
    .await;
}

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::Result;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};
    use objc2_foundation::{NSArray, NSString, NSURL};
    use std::path::PathBuf;

    pub fn read_file_list() -> Option<Vec<PathBuf>> {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            let items = pb.pasteboardItems()?;
            let mut out = Vec::new();
            for item in items.iter() {
                let Some(s) = item.stringForType(NSPasteboardTypeFileURL) else {
                    continue;
                };
                if let Some(url) = NSURL::URLWithString(&s)
                    && let Some(path) = url.path()
                {
                    out.push(PathBuf::from(path.to_string()));
                }
            }
            (!out.is_empty()).then_some(out)
        }
    }

    pub fn write_file_list(paths: &[PathBuf]) -> Result<()> {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let urls: Vec<Retained<ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>>> = paths
            .iter()
            .map(|p| {
                let url = NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy()));
                ProtocolObject::from_retained(url)
            })
            .collect();
        let array = NSArray::from_retained_slice(&urls);
        anyhow::ensure!(pb.writeObjects(&array), "NSPasteboard writeObjects 失敗");
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use anyhow::Result;
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};

    const CF_HDROP: u32 = 15; // winuser.h

    /// shellapi.h DROPFILES，20 bytes（pFiles 4 + POINT 8 + fNC 4 + fWide 4）。
    #[repr(C)]
    struct DropFiles {
        p_files: u32,
        pt: [i32; 2],
        f_nc: i32,
        f_wide: i32,
    }

    /// RAII：確保 CloseClipboard 在任何 early-return 都會跑。
    struct ClipboardGuard;
    impl ClipboardGuard {
        fn open() -> Option<Self> {
            // 其他 app 可能短暫佔住 clipboard，小退避重試。
            for _ in 0..5 {
                if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                    return Some(Self);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            None
        }
    }
    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe { CloseClipboard() };
        }
    }

    pub fn read_file_list() -> Option<Vec<PathBuf>> {
        unsafe {
            if IsClipboardFormatAvailable(CF_HDROP) == 0 {
                return None;
            }
            let _guard = ClipboardGuard::open()?;
            let h = GetClipboardData(CF_HDROP);
            if h.is_null() {
                return None;
            }
            let hdrop = h as HDROP;
            let count = DragQueryFileW(hdrop, u32::MAX, std::ptr::null_mut(), 0);
            let mut out = Vec::with_capacity(count as usize);
            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0);
                let mut buf = vec![0u16; len as usize + 1];
                let got = DragQueryFileW(hdrop, i, buf.as_mut_ptr(), len + 1);
                if got == 0 {
                    continue;
                }
                buf.truncate(got as usize);
                out.push(PathBuf::from(OsString::from_wide(&buf)));
            }
            (!out.is_empty()).then_some(out)
        }
    }

    pub fn write_file_list(paths: &[PathBuf]) -> Result<()> {
        // CF_HDROP payload：DROPFILES header + UTF-16 路徑（各自 null 結尾）+ 整串再一個 null。
        let mut wide: Vec<u16> = Vec::new();
        for p in paths {
            wide.extend(p.as_os_str().encode_wide());
            wide.push(0);
        }
        wide.push(0);

        let header = size_of::<DropFiles>();
        let total = header + wide.len() * 2;
        unsafe {
            let hglobal = GlobalAlloc(GMEM_MOVEABLE, total);
            anyhow::ensure!(!hglobal.is_null(), "GlobalAlloc 失敗");
            let ptr = GlobalLock(hglobal);
            if ptr.is_null() {
                GlobalFree(hglobal);
                anyhow::bail!("GlobalLock 失敗");
            }
            let df = DropFiles {
                p_files: header as u32,
                pt: [0, 0],
                f_nc: 0,
                f_wide: 1,
            };
            std::ptr::copy_nonoverlapping((&raw const df).cast::<u8>(), ptr.cast::<u8>(), header);
            std::ptr::copy_nonoverlapping(
                wide.as_ptr().cast::<u8>(),
                ptr.cast::<u8>().add(header),
                wide.len() * 2,
            );
            GlobalUnlock(hglobal);

            let Some(_guard) = ClipboardGuard::open() else {
                GlobalFree(hglobal);
                anyhow::bail!("OpenClipboard 失敗（被其他 app 佔住）");
            };
            EmptyClipboard();
            // SetClipboardData 成功後 hglobal 所有權歸系統，不得再 free。
            let set = SetClipboardData(CF_HDROP, hglobal as HANDLE);
            if set.is_null() {
                GlobalFree(hglobal);
                anyhow::bail!("SetClipboardData(CF_HDROP) 失敗");
            }
        }
        Ok(())
    }
}

/// 非 mac / win 平台：stub（跟 capture 的 ManualCapture 同精神）。
#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use std::path::PathBuf;

    pub fn read_file_list() -> Option<Vec<PathBuf>> {
        None
    }

    pub fn write_file_list(_paths: &[PathBuf]) -> anyhow::Result<()> {
        anyhow::bail!("此平台不支援檔案剪貼簿")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_domains_disjoint() {
        // 同一個 last_synced 槽：文字跟檔案指紋分域，不可能誤判去重。
        assert_ne!(fingerprint_text("a"), fingerprint(&[PathBuf::from("a")]));
    }

    #[test]
    fn fingerprint_files_stable() {
        let p = vec![
            PathBuf::from("/nonexistent/x"),
            PathBuf::from("/nonexistent/y"),
        ];
        assert_eq!(fingerprint(&p), fingerprint(&p));
        assert_ne!(fingerprint(&p), fingerprint(&p[..1]));
    }
}
