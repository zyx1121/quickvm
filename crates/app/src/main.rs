//! quickvm — QUIC 鍵鼠跨機分享 (v1: 合成 API 注入)。
//!
//! 角色：`serve` = 被控端（注入收到的事件）、`connect` = 主控端（捕捉本機鍵鼠並轉發）。
//! 主控端跑邊緣切換狀態機：游標撞到「朝向對端那側」的螢幕邊緣才穿過去控制對端，
//! 期間吞掉本機輸入（grab）；在對端撞回反向邊緣則交還本機。方位 / 尺寸由設定檔決定。

// Windows 用 GUI subsystem：console app 在 interactive session（schtasks 起 serve）
// 必開一個 console 視窗，誤關即殺 serve。GUI subsystem 無視窗背景跑；
// 從 terminal 手動跑的輸出由下面 attach_parent_console() 接回。
#![cfg_attr(windows, windows_subsystem = "windows")]

mod clipboard;
mod clipboard_files;
mod config;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{Config, Side};
use quickvm_capture::default_capture;
use quickvm_emulation::{EnigoEmulator, InputEmulator};
use quickvm_event::{Control, Direction, Event, Modifiers};
use quickvm_proto::Reliable;
use quickvm_transport::{BackLink, Incoming, connect, serve};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "quickvm", version, about = "QUIC 鍵鼠跨機分享 (v1)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 被控端：監聽連線，把收到的事件注入本機。
    Serve {
        #[arg(long, default_value = "0.0.0.0:7777")]
        bind: SocketAddr,
    },
    /// 主控端：讀設定檔，捕捉本機鍵鼠，邊緣切換時轉發給對端。
    Connect {
        /// 設定檔路徑（預設 ~/.config/quickvm/config.toml）。
        #[arg(long)]
        config: Option<PathBuf>,
        /// 覆蓋設定檔的 server 位址。
        #[arg(long)]
        server: Option<SocketAddr>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(windows)]
    attach_parent_console();
    tracing_subscriber::fmt::init();
    match Cli::parse().cmd {
        Cmd::Serve { bind } => run_serve(bind).await,
        Cmd::Connect { config, server } => run_connect(config, server).await,
    }
}

/// GUI subsystem 沒有 console：從 terminal 跑時 attach 回父 console 讓輸出 / clap
/// 錯誤訊息可見；由 task / 雙擊啟動（父無 console）attach 失敗 → 維持無視窗背景。
/// 已被 redirect 的 std handle（task 的 `> log`）不受 attach 影響，照寫檔案。
#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// 共享密鑰（PSK）：兩端必須相同。從環境變數 `QUICKVM_SECRET` 讀，未設則拒絕啟動
/// —— 沒有它就是「任何人連上即可控制本機」，不允許靜默以不安全模式跑。
fn load_secret() -> Result<Vec<u8>> {
    let s = std::env::var("QUICKVM_SECRET").map_err(|_| {
        anyhow::anyhow!(
            "未設 QUICKVM_SECRET —— 兩端需設相同的共享密鑰（建議 32+ 字元隨機字串）。\n\
             例：export QUICKVM_SECRET=\"$(openssl rand -hex 32)\""
        )
    })?;
    if s.len() < 16 {
        anyhow::bail!("QUICKVM_SECRET 太短（<16 字元），請用更長的隨機字串");
    }
    Ok(s.into_bytes())
}

/// 被控端：注入收到的事件。
async fn run_serve(bind: SocketAddr) -> Result<()> {
    let secret = load_secret()?;
    let mut emu = EnigoEmulator::new()?;
    quickvm_transport::clean_recv_root();
    let mut rx = serve(bind, secret).await?;
    // 回送句柄（Leave 時回推剪貼簿）+ 已同步指紋（避免重複互寫 / 回聲）。
    let mut back: Option<BackLink> = None;
    let mut last_synced: Option<u64> = None;
    tracing::info!(%bind, "serve (被控端) — 等待主控連入…");
    while let Some(item) = rx.recv().await {
        match item {
            Incoming::Connected(bl) => back = Some(bl),
            Incoming::Motion(m) => emu.emit(&Event::MotionAbs { x: m.x, y: m.y })?,
            Incoming::Reliable(Reliable::Input(ev)) => emu.emit(&ev)?,
            // 主控端推來的剪貼簿 → 寫進本機。
            Incoming::Reliable(Reliable::Clipboard(text)) => {
                last_synced = Some(clipboard_files::fingerprint_text(&text));
                clipboard::write_text(text).await;
                tracing::info!("剪貼簿已同步（主控端 → 本機）");
            }
            // 主控端推來的檔案（內容已落地暫存）→ 剪貼簿指過去。
            Incoming::Files(paths) => {
                last_synced = Some(clipboard_files::fingerprint(&paths));
                let n = paths.len();
                clipboard_files::write_files(paths).await;
                tracing::info!(files = n, "檔案剪貼簿已同步（主控端 → 本機）");
            }
            Incoming::Reliable(Reliable::Control(c)) => match c {
                // 進入：定位游標到進入點，並同步主控端當下按住的修飾鍵（避免黏鍵 / 漏修飾）。
                Control::Enter { x, y, mods } => {
                    emu.emit(&Event::MotionAbs { x, y })?;
                    for (bit, usage) in MODIFIER_USAGES {
                        if mods.0 & bit != 0 {
                            emu.emit(&Event::Key {
                                usage,
                                dir: Direction::Press,
                            })?;
                        }
                    }
                }
                // 離開：放開所有按住的鍵，控制權交還主控端，把被控期間複製的東西帶回去。
                Control::Leave => {
                    emu.release_all()?;
                    sync_clipboard_back(back.as_ref(), &mut last_synced).await;
                    tracing::info!("leave — 控制交還主控端");
                }
            },
            // 主控斷線：release 殘留按鍵，等下一個主控連入。
            Incoming::Disconnected => {
                emu.release_all()?;
                back = None;
                tracing::info!("主控斷線 — 已放開所有按鍵");
            }
        }
    }
    Ok(())
}

/// 被控端 Leave 時回推剪貼簿（檔案優先，再文字）：跟上次同步不同才送。
/// 檔案大、傳輸久 —— spawn 背景送，不卡 serve loop 處理下一次 Enter。
async fn sync_clipboard_back(back: Option<&BackLink>, last_synced: &mut Option<u64>) {
    let Some(bl) = back else { return };
    if let Some(paths) = clipboard_files::read_files().await {
        let fp = clipboard_files::fingerprint(&paths);
        if *last_synced == Some(fp) {
            return;
        }
        *last_synced = Some(fp);
        let bl = bl.clone();
        tokio::spawn(async move {
            match bl.send_files(&paths).await {
                Ok(()) => tracing::info!(files = paths.len(), "檔案已回送（本機 → 主控端）"),
                Err(e) => tracing::warn!(error = %e, "檔案回送失敗"),
            }
        });
        return;
    }
    let Some(text) = clipboard::read_text().await else {
        return;
    };
    let fp = clipboard_files::fingerprint_text(&text);
    if *last_synced == Some(fp) {
        return;
    }
    *last_synced = Some(fp);
    match bl.send_reliable(&Reliable::Clipboard(text)).await {
        Ok(()) => tracing::info!("剪貼簿已回送（本機 → 主控端）"),
        Err(e) => tracing::warn!(error = %e, "剪貼簿回送失敗"),
    }
}

/// 主控端：邊緣切換狀態機。
async fn run_connect(config: Option<PathBuf>, server: Option<SocketAddr>) -> Result<()> {
    let cfg = Config::load(config)?;
    let addr: SocketAddr = match server {
        Some(a) => a,
        None => cfg
            .server
            .parse()
            .with_context(|| format!("config.server 不是合法位址：{}", cfg.server))?,
    };

    let secret = load_secret()?;
    quickvm_transport::clean_recv_root();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut cap = default_capture();
    cap.start(tx)?;

    // 已同步剪貼簿指紋：放重連 loop 外，斷線重連不重推沒變的內容。
    let mut last_synced: Option<u64> = None;

    // 斷線自動重連：WiFi 抖動 / 睡眠喚醒 / serve 重啟都會讓 QUIC 判死，不該要求手動重起。
    loop {
        let (link, mut back_rx) = match connect(addr, secret.clone()).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "連不上被控端，2 秒後重試…");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };
        tracing::info!(%addr, side = ?cfg.remote.side, "connect (主控端) — 邊緣切換已啟用（Ctrl+C 結束）");
        if let Err(e) = drive(
            link,
            &mut rx,
            &mut back_rx,
            &cfg,
            cap.as_mut(),
            &mut last_synced,
        )
        .await
        {
            // 斷線必須交還本機（ungrab）—— 否則 Remote 狀態下本機輸入持續被吞，Mac 整台鎖死。
            cap.set_grab(false);
            tracing::warn!(error = %e, "連線中斷 — 已交還本機，重連中…");
        }
    }
}

/// 一條連線存活期間的事件迴圈；連線層錯誤回 Err 交給外層重連。
async fn drive(
    mut link: quickvm_transport::Link,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    back_rx: &mut tokio::sync::mpsc::Receiver<Incoming>,
    cfg: &Config,
    cap: &mut dyn quickvm_capture::InputCapture,
    last_synced: &mut Option<u64>,
) -> Result<()> {
    let mut target = Target::Local;
    // Remote 模式下的虛擬游標（對端像素座標）。
    let (mut vx, mut vy) = (0.0_f64, 0.0_f64);
    let mut mods = Modifiers::default();
    // 待送的最新虛擬游標：capture 高頻更新它，固定 tick 才送出 → coalesce 掉中間點，
    // WiFi spike 時不會把積壓的舊位置全噴出去，spike 一過直接跳最新。
    let mut pending: Option<(f64, f64)> = None;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(8)); // 125Hz
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            // 公平輪詢：不能 biased，否則高頻事件會餓死 ticker → pending 永不送、游標不動。
            maybe = rx.recv() => {
                let Some(ev) = maybe else { anyhow::bail!("capture channel closed") };
                match target {
                    // 本機自用：只看滑鼠絕對位置有沒有撞到「朝向對端」的邊緣。
                    Target::Local => {
                        if let Event::Key { usage, dir } = ev {
                            update_mods(&mut mods, usage, dir); // Local：鍵盤不轉發（吞掉）
                        }
                        if let Event::MotionAbs { x, y } = ev
                            && let Some((ex, ey)) = enter_point(cfg.remote.side, x, y)
                        {
                            vx = ex * cfg.remote.width;
                            vy = ey * cfg.remote.height;
                            link.send_reliable(&Reliable::Control(Control::Enter { x: ex, y: ey, mods })).await?;
                            cap.set_grab(true);
                            target = Target::Remote;
                            tracing::info!(ex, ey, "→ 進入對端");
                            // 剪貼簿跟著切換推過去（沒變不重送），檔案優先於文字。
                            // Enter 先送顧切換體感；檔案大、傳輸久 → spawn 背景送不卡 loop。
                            if let Some(paths) = clipboard_files::read_files().await {
                                let fp = clipboard_files::fingerprint(&paths);
                                if *last_synced != Some(fp) {
                                    *last_synced = Some(fp);
                                    let fl = link.files_link();
                                    tokio::spawn(async move {
                                        match fl.send_files(&paths).await {
                                            Ok(()) => tracing::info!(files = paths.len(), "檔案已推送（本機 → 被控端）"),
                                            Err(e) => tracing::warn!(error = %e, "檔案推送失敗"),
                                        }
                                    });
                                }
                            } else if let Some(text) = clipboard::read_text().await {
                                let fp = clipboard_files::fingerprint_text(&text);
                                if *last_synced != Some(fp) {
                                    *last_synced = Some(fp);
                                    link.send_reliable(&Reliable::Clipboard(text)).await?;
                                    tracing::info!("剪貼簿已推送（本機 → 被控端）");
                                }
                            }
                        }
                    }
                    // 控制對端：累積相對位移算虛擬游標，撞回反向邊緣則交還本機。
                    Target::Remote => match ev {
                        Event::MotionRel { dx, dy } => {
                            vx += dx;
                            vy += dy;
                            if left_remote(cfg.remote.side, vx, vy, cfg.remote.width, cfg.remote.height) {
                                link.send_reliable(&Reliable::Control(Control::Leave)).await?;
                                cap.set_grab(false);
                                target = Target::Local;
                                pending = None;
                                tracing::info!("← 交還本機");
                            } else {
                                vx = vx.clamp(0.0, cfg.remote.width);
                                vy = vy.clamp(0.0, cfg.remote.height);
                                pending = Some((vx, vy)); // 不立即送，等 tick coalesce
                            }
                        }
                        Event::Key { usage, dir } => {
                            update_mods(&mut mods, usage, dir);
                            link.send_reliable(&Reliable::Input(Event::Key { usage, dir })).await?;
                        }
                        Event::MotionAbs { .. } => {} // grab 模式不該出現
                        other => link.send_reliable(&Reliable::Input(other)).await?,
                    },
                }
            }
            _ = ticker.tick() => {
                if let Some((x, y)) = pending.take() {
                    link.send_motion(x / cfg.remote.width, y / cfg.remote.height)?;
                }
            }
            // 被控端的回送（Leave 時回推剪貼簿 / 檔案）。channel 關閉 = 連線死，
            // 回 Err 走外層重連 —— 不能留在 select 裡（closed channel 立即 ready → busy loop）。
            maybe = back_rx.recv() => {
                match maybe {
                    Some(Incoming::Reliable(Reliable::Clipboard(text))) => {
                        *last_synced = Some(clipboard_files::fingerprint_text(&text));
                        clipboard::write_text(text).await;
                        tracing::info!("剪貼簿已同步（被控端 → 本機）");
                    }
                    Some(Incoming::Files(paths)) => {
                        *last_synced = Some(clipboard_files::fingerprint(&paths));
                        let n = paths.len();
                        clipboard_files::write_files(paths).await;
                        tracing::info!(files = n, "檔案剪貼簿已同步（被控端 → 本機）");
                    }
                    Some(_) => {} // 其餘 variant 不會從回送通道出現
                    None => anyhow::bail!("回送 channel 關閉（連線結束）"),
                }
            }
        }
    }
}

enum Target {
    Local,
    Remote,
}

/// 邊緣判定閾值（正規化）。撞到底時 capture 送的絕對座標會被 clamp 到 0/1。
const EDGE: f64 = 0.001;

/// Local 撞到「朝向對端」的邊緣 → 回傳進入對端的正規化座標（沿用 cross-axis 比例）。
fn enter_point(side: Side, x: f64, y: f64) -> Option<(f64, f64)> {
    match side {
        Side::Right if x >= 1.0 - EDGE => Some((EDGE, y)), // 從對端左緣進
        Side::Left if x <= EDGE => Some((1.0 - EDGE, y)),  // 從對端右緣進
        Side::Bottom if y >= 1.0 - EDGE => Some((x, EDGE)), // 從對端上緣進
        Side::Top if y <= EDGE => Some((x, 1.0 - EDGE)),   // 從對端下緣進
        _ => None,
    }
}

/// Remote 虛擬游標越過「朝向本機」的邊界 → 該交還本機。
fn left_remote(side: Side, vx: f64, vy: f64, w: f64, h: f64) -> bool {
    match side {
        Side::Right => vx < 0.0, // 對端在右 → 本機在對端左 → 撞左緣回去
        Side::Left => vx > w,
        Side::Bottom => vy < 0.0,
        Side::Top => vy > h,
    }
}

/// `Modifiers` bit ↔ 代表性的 HID usage（左鍵），serve 端進入時據此同步修飾鍵。
const MODIFIER_USAGES: [(u16, u16); 4] = [
    (Modifiers::CTRL, 0xE0),
    (Modifiers::SHIFT, 0xE1),
    (Modifiers::ALT, 0xE2),
    (Modifiers::META, 0xE3),
];

/// 維護修飾鍵 bitflags（給 Enter 帶過去，讓對端對齊避免黏鍵）。
fn update_mods(mods: &mut Modifiers, usage: u16, dir: Direction) {
    let bit = match usage {
        0xE0 | 0xE4 => Modifiers::CTRL,
        0xE1 | 0xE5 => Modifiers::SHIFT,
        0xE2 | 0xE6 => Modifiers::ALT,
        0xE3 | 0xE7 => Modifiers::META,
        _ => return,
    };
    match dir {
        Direction::Press => mods.0 |= bit,
        Direction::Release => mods.0 &= !bit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── enter_point：撞到「朝向對端」的邊緣才穿過去，並落在對端的相反邊 ──
    // 撞到底時 capture 會把絕對座標 clamp 到 0/1，所以用 0.0 / 1.0 當「撞到」輸入。

    #[test]
    fn enter_point_right_enters_remote_left_edge() {
        // 對端在右 → 從對端左緣（EDGE）進，y 比例沿用（cross-axis 保留）。
        assert_eq!(enter_point(Side::Right, 1.0, 0.3), Some((EDGE, 0.3)));
    }

    #[test]
    fn enter_point_left_enters_remote_right_edge() {
        assert_eq!(enter_point(Side::Left, 0.0, 0.7), Some((1.0 - EDGE, 0.7)));
    }

    #[test]
    fn enter_point_bottom_enters_remote_top_edge() {
        assert_eq!(enter_point(Side::Bottom, 0.4, 1.0), Some((0.4, EDGE)));
    }

    #[test]
    fn enter_point_top_enters_remote_bottom_edge() {
        assert_eq!(enter_point(Side::Top, 0.6, 0.0), Some((0.6, 1.0 - EDGE)));
    }

    #[test]
    fn enter_point_center_never_crosses() {
        for side in [Side::Right, Side::Left, Side::Top, Side::Bottom] {
            assert_eq!(enter_point(side, 0.5, 0.5), None);
        }
    }

    #[test]
    fn enter_point_wrong_edge_does_not_cross() {
        // 撞到「非朝向對端」的邊緣不能穿——否則游標一碰任何邊就被吸走。
        assert_eq!(enter_point(Side::Right, 0.0, 0.5), None); // 反向邊
        assert_eq!(enter_point(Side::Right, 0.5, 1.0), None); // 正交邊
        assert_eq!(enter_point(Side::Left, 1.0, 0.5), None);
        assert_eq!(enter_point(Side::Bottom, 0.5, 0.0), None);
        assert_eq!(enter_point(Side::Top, 0.5, 1.0), None);
    }

    // ── left_remote：虛擬游標（對端像素座標）越過「朝向本機」的邊界才交還 ──

    const W: f64 = 1920.0;
    const H: f64 = 1080.0;

    #[test]
    fn left_remote_right_returns_via_remote_left_edge() {
        // 對端在右 → 本機在對端左 → vx 越過 0 才回去；貼著 0 還算在對端。
        assert!(left_remote(Side::Right, -0.1, 540.0, W, H));
        assert!(!left_remote(Side::Right, 0.0, 540.0, W, H));
        assert!(!left_remote(Side::Right, W, 540.0, W, H)); // 遠端另一側只 clamp 不交還
    }

    #[test]
    fn left_remote_left_returns_via_remote_right_edge() {
        assert!(left_remote(Side::Left, W + 0.1, 540.0, W, H));
        assert!(!left_remote(Side::Left, W, 540.0, W, H));
        assert!(!left_remote(Side::Left, 0.0, 540.0, W, H));
    }

    #[test]
    fn left_remote_bottom_returns_via_remote_top_edge() {
        assert!(left_remote(Side::Bottom, 960.0, -0.1, W, H));
        assert!(!left_remote(Side::Bottom, 960.0, 0.0, W, H));
        assert!(!left_remote(Side::Bottom, 960.0, H, W, H));
    }

    #[test]
    fn left_remote_top_returns_via_remote_bottom_edge() {
        assert!(left_remote(Side::Top, 960.0, H + 0.1, W, H));
        assert!(!left_remote(Side::Top, 960.0, H, W, H));
        assert!(!left_remote(Side::Top, 960.0, 0.0, W, H));
    }

    // ── update_mods：HID usage 0xE0–0xE7 維護 Modifiers bitflags ──

    #[test]
    fn update_mods_press_sets_and_release_clears_each_modifier() {
        // 左右修飾鍵各自的 usage 都要映到同一個 bit——漏任何一個 = Enter 帶過去的狀態缺角。
        let cases: [(u16, u16); 8] = [
            (0xE0, Modifiers::CTRL),
            (0xE1, Modifiers::SHIFT),
            (0xE2, Modifiers::ALT),
            (0xE3, Modifiers::META),
            (0xE4, Modifiers::CTRL),
            (0xE5, Modifiers::SHIFT),
            (0xE6, Modifiers::ALT),
            (0xE7, Modifiers::META),
        ];
        for (usage, bit) in cases {
            let mut mods = Modifiers::default();
            update_mods(&mut mods, usage, Direction::Press);
            assert_eq!(mods.0, bit, "press 0x{usage:02X} 應設位");
            update_mods(&mut mods, usage, Direction::Release);
            assert_eq!(mods.0, 0, "release 0x{usage:02X} 應清位");
        }
    }

    #[test]
    fn update_mods_ignores_non_modifier_usage() {
        // 一般按鍵（如 0x04 = A）不能動到 bitflags——否則打字會污染修飾鍵狀態。
        let mut mods = Modifiers(Modifiers::CTRL | Modifiers::SHIFT);
        update_mods(&mut mods, 0x04, Direction::Press);
        update_mods(&mut mods, 0x04, Direction::Release);
        assert_eq!(mods.0, Modifiers::CTRL | Modifiers::SHIFT);
    }

    #[test]
    fn update_mods_stacks_multiple_modifiers() {
        let mut mods = Modifiers::default();
        update_mods(&mut mods, 0xE0, Direction::Press); // L-Ctrl
        update_mods(&mut mods, 0xE1, Direction::Press); // L-Shift
        update_mods(&mut mods, 0xE3, Direction::Press); // L-Meta
        assert_eq!(mods.0, Modifiers::CTRL | Modifiers::SHIFT | Modifiers::META);
        // 放開其中一個不影響其餘。
        update_mods(&mut mods, 0xE1, Direction::Release);
        assert_eq!(mods.0, Modifiers::CTRL | Modifiers::META);
        // 左右同名鍵共用一個 bit：右 Ctrl release 會清掉左 Ctrl 設的位（現行語意）。
        update_mods(&mut mods, 0xE4, Direction::Release);
        assert_eq!(mods.0, Modifiers::META);
    }
}
