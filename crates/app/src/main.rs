//! quickvm — QUIC 鍵鼠跨機分享 (v1: 合成 API 注入)。
//!
//! 角色：`serve` = 被控端（注入收到的事件）、`connect` = 主控端（捕捉本機鍵鼠並轉發）。

use anyhow::Result;
use clap::{Parser, Subcommand};
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { bind } => {
            tracing::info!(%bind, "serve (被控端) — TODO: wire transport::serve → emulation::emit");
            // let mut emu = quickvm_emulation::EnigoEmulator::new()?;
            // quickvm_transport::serve(bind).await?;  // recv → emu.emit(&ev)
        }
        Cmd::Connect { addr } => {
            tracing::info!(%addr, "connect (主控端) — TODO: wire capture → transport::connect");
            // let mut link = quickvm_transport::connect(addr).await?;  // capture → link.send_*
        }
    }
    Ok(())
}
