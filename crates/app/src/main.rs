//! quickvm — QUIC 鍵鼠跨機分享 (v1: 合成 API 注入)。
//!
//! 角色：`serve` = 被控端（注入收到的事件）、`connect` = 主控端（捕捉本機鍵鼠並轉發）。
//! v1 demo：`connect` 送一串假事件（移到螢幕中央 + 左鍵點一下）證明端到端鏈路通；
//! 真實的本機鍵鼠 capture 待 `quickvm-capture` 平台後端（見該 crate TODO）。

use anyhow::Result;
use clap::{Parser, Subcommand};
use quickvm_emulation::{EnigoEmulator, InputEmulator};
use quickvm_event::{Direction, Event, MouseButton};
use quickvm_proto::Reliable;
use quickvm_transport::{connect, serve, Incoming};
use std::net::SocketAddr;
use std::time::Duration;

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
    /// 主控端：連到被控端，轉發事件（v1：送 demo 假事件）。
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
            let mut link = connect(addr).await?;
            tracing::info!(%addr, "connect (主控端) — 送 demo：移到螢幕中央 + 左鍵點一下");
            link.send_motion(0.5, 0.5)?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            link.send_reliable(&Reliable::Input(Event::Button {
                button: MouseButton::Left,
                dir: Direction::Press,
            }))
            .await?;
            link.send_reliable(&Reliable::Input(Event::Button {
                button: MouseButton::Left,
                dir: Direction::Release,
            }))
            .await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            tracing::info!("demo 事件已送出");
        }
    }
    Ok(())
}
