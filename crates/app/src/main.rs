//! quickvm — QUIC 鍵鼠跨機分享 (v1: 合成 API 注入)。
//!
//! 角色：`serve` = 被控端（注入收到的事件）、`connect` = 主控端（捕捉本機鍵鼠並轉發）。
//! macOS 主控端用 CGEventTap 真捕捉滑鼠（鍵盤待 HID usage 映射）；被控端用 enigo 注入。

use anyhow::Result;
use clap::{Parser, Subcommand};
use quickvm_capture::default_capture;
use quickvm_emulation::{EnigoEmulator, InputEmulator};
use quickvm_event::Event;
use quickvm_proto::Reliable;
use quickvm_transport::{connect, serve, Incoming};
use std::net::SocketAddr;

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
    /// 主控端：連到被控端，捕捉本機鍵鼠並轉發。
    Connect {
        /// 被控端位址，如 192.168.1.121:7777
        addr: SocketAddr,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    match Cli::parse().cmd {
        Cmd::Serve { bind } => {
            let mut emu = EnigoEmulator::new()?;
            let mut rx = serve(bind).await?;
            tracing::info!(%bind, "serve (被控端) — 等待主控連入…");
            while let Some(item) = rx.recv().await {
                match item {
                    Incoming::Motion(m) => emu.emit(&Event::MotionAbs { x: m.x, y: m.y })?,
                    Incoming::Reliable(Reliable::Input(ev)) => emu.emit(&ev)?,
                    Incoming::Reliable(Reliable::Control(c)) => tracing::debug!(?c, "control"),
                }
            }
        }
        Cmd::Connect { addr } => {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let mut cap = default_capture();
            cap.start(tx)?;
            let mut link = connect(addr).await?;
            tracing::info!(%addr, "connect (主控端) — 捕捉本機鍵鼠並轉發（Ctrl+C 結束）");
            while let Some(ev) = rx.recv().await {
                match ev {
                    Event::MotionAbs { x, y } => link.send_motion(x, y)?,
                    other => {
                        tracing::debug!(?other, "reliable →");
                        link.send_reliable(&Reliable::Input(other)).await?;
                    }
                }
            }
        }
    }
    Ok(())
}
