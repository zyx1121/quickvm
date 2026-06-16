//! Windows 系統匣（tray）控制面 — 給無視窗的 serve 背景程序一個可見開關。
//!
//! 對稱於 macOS 的 QuicKVM.app menubar，但反過來：不另開殼，tray 直接住在 quickvm
//! binary（`quickvm tray` 子命令）。啟動時自動拉起 `quickvm serve` child，右鍵選單
//! 啟動 / 停止 / 開 log / 結束。視窗 proc 是無 capture 的 `extern "system" fn`
//! （對齊 capture/windows.rs），靠 module-level static 取狀態。
//!
//! 為何 supervise child 而非 in-process：serve 只注入、無 grab 狀態，kill 絕對安全；
//! child 繼承 tray 的 session（AtLogOn task 給 session 1，enigo 注入才有效），且 tray
//! 本身不需 tokio runtime。`DESIRED` 旗標區分「使用者按停止」與「serve 自己掛了」——
//! 看門狗 timer 只在前者為 true、後者發生時重拉（取代舊 schtasks RestartCount）。

use anyhow::Result;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::sync::{Mutex, OnceLock};
use std::{mem, ptr};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, IDI_APPLICATION, LoadIconW, MF_SEPARATOR,
    MF_STRING, MSG, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    SetTimer, TPM_RIGHTALIGN, TrackPopupMenu, TranslateMessage, WM_APP, WM_COMMAND, WM_DESTROY,
    WM_LBUTTONUP, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW,
};

/// tray icon 點擊回呼訊息（自訂，WM_APP 之上）。
const WM_TRAY: u32 = WM_APP + 1;
const TRAY_UID: u32 = 1;
const ID_TOGGLE: u32 = 1;
const ID_LOG: u32 = 2;
const ID_QUIT: u32 = 3;

static CHILD: Mutex<Option<Child>> = Mutex::new(None);
/// 使用者期望 serve 在跑（按停止 → false）。看門狗只在這為 true 時重拉，否則「停止」會被秒拉回。
static DESIRED: AtomicBool = AtomicBool::new(false);
/// explorer.exe 重啟會清掉所有 tray 圖示並廣播 TaskbarCreated；收到就重加，否則 daemon 跑久了圖示會消失。
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static EXE: OnceLock<PathBuf> = OnceLock::new();
static BIND: OnceLock<String> = OnceLock::new();
static SERVE_LOG: OnceLock<Option<PathBuf>> = OnceLock::new();

pub fn run(bind: SocketAddr, serve_log: Option<PathBuf>) -> Result<()> {
    EXE.set(std::env::current_exe()?).ok();
    BIND.set(bind.to_string()).ok();
    SERVE_LOG.set(serve_log).ok();

    unsafe {
        let hinstance = GetModuleHandleW(ptr::null());
        let class = wide("QuicKVMTray");
        let mut wc: WNDCLASSW = mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinstance as _;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);

        // explorer 重啟後靠這個廣播訊息重加圖示（message-only 視窗收不到廣播，故用一般 top-level）。
        TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), SeqCst);

        let name = wide("QuicKVM");
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW, // 不進工作列 / alt-tab
            class.as_ptr(),
            name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            ptr::null_mut(), // 一般 top-level 隱藏視窗（不 ShowWindow）：收得到 TaskbarCreated 廣播、可被 FindWindow 找到
            ptr::null_mut(),
            hinstance as _,
            ptr::null(),
        );
        if hwnd.is_null() {
            anyhow::bail!("CreateWindowExW 失敗");
        }

        add_icon(hwnd);
        start_serve(); // 啟動即拉起 serve（對稱 mac 授權後 auto-start）
        update_tip(hwnd);
        SetTimer(hwnd, 1, 5000, None); // 看門狗：serve 掉了自動重拉

        let mut msg: MSG = mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    stop_serve();
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // explorer 重啟 → 重加圖示（動態註冊的訊息 id，無法當 const match）。
    let tc = TASKBAR_CREATED.load(SeqCst);
    if tc != 0 && msg == tc {
        add_icon(hwnd);
        update_tip(hwnd);
        return 0;
    }
    match msg {
        WM_TRAY => {
            // 非 v4 通知：lParam 低字為滑鼠訊息。左 / 右鍵都彈選單。
            let evt = (lparam as u32) & 0xFFFF;
            if evt == WM_RBUTTONUP || evt == WM_LBUTTONUP {
                show_menu(hwnd);
            }
            0
        }
        WM_COMMAND => {
            match (wparam as u32) & 0xFFFF {
                ID_TOGGLE => {
                    if DESIRED.load(SeqCst) {
                        stop_serve();
                    } else {
                        start_serve();
                    }
                    update_tip(hwnd);
                }
                ID_LOG => open_log(),
                ID_QUIT => unsafe { DestroyWindow(hwnd); },
                _ => {}
            }
            0
        }
        WM_TIMER => {
            if DESIRED.load(SeqCst) && !is_running() {
                ensure_running();
                update_tip(hwnd);
            }
            0
        }
        WM_DESTROY => {
            remove_icon(hwnd);
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn add_icon(hwnd: HWND) {
    unsafe {
        let mut nid = base_nid(hwnd);
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = LoadIconW(ptr::null_mut(), IDI_APPLICATION);
        set_tip(&mut nid, "QuicKVM serve");
        Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

fn update_tip(hwnd: HWND) {
    let mut nid = base_nid(hwnd);
    nid.uFlags = NIF_TIP;
    set_tip(
        &mut nid,
        if is_running() {
            "QuicKVM serve — 執行中"
        } else {
            "QuicKVM serve — 已停止"
        },
    );
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
}

fn remove_icon(hwnd: HWND) {
    let nid = base_nid(hwnd);
    unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let menu = CreatePopupMenu();
        let toggle = wide(if DESIRED.load(SeqCst) { "停止" } else { "啟動" });
        AppendMenuW(menu, MF_STRING, ID_TOGGLE as usize, toggle.as_ptr());
        let log = wide("開啟 log");
        AppendMenuW(menu, MF_STRING, ID_LOG as usize, log.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        let quit = wide("結束");
        AppendMenuW(menu, MF_STRING, ID_QUIT as usize, quit.as_ptr());
        // TrackPopupMenu 前要 SetForegroundWindow，否則點外面選單不關（MSDN）。
        let mut pt: POINT = mem::zeroed();
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        TrackPopupMenu(menu, TPM_RIGHTALIGN, pt.x, pt.y, 0, hwnd, ptr::null());
        DestroyMenu(menu);
    }
}

fn base_nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { mem::zeroed() };
    nid.cbSize = mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    nid
}

fn set_tip(nid: &mut NOTIFYICONDATAW, s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(127).collect();
    nid.szTip[..w.len()].copy_from_slice(&w);
    nid.szTip[w.len()] = 0;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 使用者開：標記期望 + 確保在跑。
fn start_serve() {
    DESIRED.store(true, SeqCst);
    ensure_running();
}

/// 使用者關:標記不期望 + 殺掉 child（serve 純注入、無 grab，kill 安全）。
fn stop_serve() {
    DESIRED.store(false, SeqCst);
    if let Some(mut c) = CHILD.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// 沒在跑就 spawn 一個 serve child（不改 DESIRED，供看門狗重拉用）。
fn ensure_running() {
    if is_running() {
        return;
    }
    let Some(exe) = EXE.get() else { return };
    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .arg("--bind")
        .arg(BIND.get().map(String::as_str).unwrap_or("0.0.0.0:7777"));
    if let Some(Some(log)) = SERVE_LOG.get() {
        cmd.arg("--log-file").arg(log);
    }
    match cmd.spawn() {
        Ok(child) => *CHILD.lock().unwrap() = Some(child),
        Err(e) => tracing::error!(error = %e, "啟動 serve child 失敗"),
    }
}

fn is_running() -> bool {
    // try_wait Ok(None) = 還在跑；Ok(Some) = 已退出；None = 從沒 spawn 過。
    matches!(
        CHILD.lock().unwrap().as_mut().map(|c| c.try_wait()),
        Some(Ok(None))
    )
}

fn open_log() {
    if let Some(Some(log)) = SERVE_LOG.get() {
        let _ = Command::new("notepad.exe").arg(log).spawn();
    }
}
