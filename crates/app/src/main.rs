//! quickvm — QUIC 鍵鼠跨機分享 (v1: 合成 API 注入)。
//!
//! 角色：`serve` = 被控端（注入收到的事件）、`connect` = 主控端（捕捉本機鍵鼠並轉發）。
//! 主控端跑邊緣切換狀態機：游標撞到「朝向對端那側」的螢幕邊緣才穿過去控制對端，
//! 期間吞掉本機輸入（grab）；在對端撞回反向邊緣則交還本機。方位 / 尺寸由設定檔決定。

mod config;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{Config, Side};
use quickvm_capture::default_capture;
use quickvm_emulation::{EnigoEmulator, InputEmulator};
use quickvm_event::{Control, Direction, Event, Modifiers};
use quickvm_proto::Reliable;
use quickvm_transport::{connect, serve, Incoming};
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
    tracing_subscriber::fmt::init();
    match Cli::parse().cmd {
        Cmd::Serve { bind } => run_serve(bind).await,
        Cmd::Connect { config, server } => run_connect(config, server).await,
    }
}

/// 被控端：注入收到的事件。
async fn run_serve(bind: SocketAddr) -> Result<()> {
    let mut emu = EnigoEmulator::new()?;
    let mut rx = serve(bind).await?;
    tracing::info!(%bind, "serve (被控端) — 等待主控連入…");
    while let Some(item) = rx.recv().await {
        match item {
            Incoming::Motion(m) => emu.emit(&Event::MotionAbs { x: m.x, y: m.y })?,
            Incoming::Reliable(Reliable::Input(ev)) => emu.emit(&ev)?,
            Incoming::Reliable(Reliable::Control(c)) => match c {
                // 進入：把游標定位到進入點，承接後續鍵鼠。
                Control::Enter { x, y, .. } => emu.emit(&Event::MotionAbs { x, y })?,
                // 離開：控制權交還主控端（TODO: release 所有按鍵避免黏鍵）。
                Control::Leave => tracing::info!("leave — 控制交還主控端"),
                _ => {}
            },
        }
    }
    Ok(())
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

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut cap = default_capture();
    cap.start(tx)?;

    // 斷線自動重連：WiFi 抖動 / 睡眠喚醒 / serve 重啟都會讓 QUIC 判死，不該要求手動重起。
    loop {
        let link = match connect(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "連不上被控端，2 秒後重試…");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };
        tracing::info!(%addr, side = ?cfg.remote.side, "connect (主控端) — 邊緣切換已啟用（Ctrl+C 結束）");
        if let Err(e) = drive(link, &mut rx, &cfg, cap.as_mut()).await {
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
    cfg: &Config,
    cap: &mut dyn quickvm_capture::InputCapture,
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
                        if let Event::MotionAbs { x, y } = ev {
                            if let Some((ex, ey)) = enter_point(cfg.remote.side, x, y) {
                                vx = ex * cfg.remote.width;
                                vy = ey * cfg.remote.height;
                                link.send_reliable(&Reliable::Control(Control::Enter { x: ex, y: ey, mods })).await?;
                                cap.set_grab(true);
                                target = Target::Remote;
                                tracing::info!(ex, ey, "→ 進入對端");
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
