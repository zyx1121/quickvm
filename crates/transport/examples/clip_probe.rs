//! 剪貼簿同步 E2E probe —— 對著真 `quickvm serve` 驗整條鏈路：
//! 推送寫入、Leave 回送、去重不重送。
//!
//! 本機模式（serve 跟 probe 同機，pbpaste/pbcopy 自動驗證，macOS only）：
//!   QUICKVM_SECRET=… quickvm serve --bind 127.0.0.1:7777
//!   QUICKVM_SECRET=… cargo run -p quickvm-transport --example clip_probe
//!
//! 跨機模式（serve 在對端，驗證點在對端剪貼簿，由外部配合讀寫）：
//!   cargo run -p quickvm-transport --example clip_probe -- <addr>:7777 --remote 10
//!   流程：推送（印 PUSHED <text>，外部驗對端剪貼簿）→ 等 N 秒（外部把對端剪貼簿
//!   改成新內容）→ Leave 收回送（印 REPLY <text>，外部比對）→ 再 Leave 驗去重。

use anyhow::{Context, Result, bail, ensure};
use quickvm_event::Control;
use quickvm_proto::Reliable;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

fn pbpaste() -> Result<String> {
    let out = Command::new("pbpaste").output()?;
    Ok(String::from_utf8(out.stdout)?)
}

fn pbcopy(text: &str) -> Result<()> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
    child
        .stdin
        .take()
        .context("pbcopy stdin")?
        .write_all(text.as_bytes())?;
    ensure!(child.wait()?.success(), "pbcopy failed");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let addr = args
        .get(1)
        .filter(|s| !s.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:7777".into())
        .parse()?;
    let remote_wait: Option<u64> = args
        .windows(2)
        .find(|w| w[0] == "--remote")
        .map(|w| w[1].parse())
        .transpose()?;

    let secret = std::env::var("QUICKVM_SECRET")
        .context("需要 QUICKVM_SECRET（跟 serve 同值）")?
        .into_bytes();
    let (link, mut back_rx) = quickvm_transport::connect(addr, secret).await?;
    println!("CONNECTED {addr}");
    let nonce = std::process::id();

    // 1. controller→serve 推送：serve 應把內容寫進它本機的剪貼簿。
    let push = format!("quickvm-e2e-push-{nonce}");
    link.send_reliable(&Reliable::Clipboard(push.clone()))
        .await?;
    println!("PUSHED {push}");

    if let Some(wait) = remote_wait {
        // 跨機：給外部時間驗對端剪貼簿 = push、再把對端剪貼簿改成新內容。
        tokio::time::sleep(Duration::from_secs(wait)).await;
    } else {
        // 本機：直接驗 + 換內容。
        tokio::time::sleep(Duration::from_millis(500)).await;
        let got = pbpaste()?;
        ensure!(got == push, "推送後 pbpaste = {got:?}，預期 {push:?}");
        println!("PASS 1 — 推送已寫入被控端剪貼簿");
        pbcopy(&format!("quickvm-e2e-reply-{nonce}"))?;
    }

    // 2. serve→controller 回送：對端剪貼簿已是新內容，Leave 應回送它。
    link.send_reliable(&Reliable::Control(Control::Leave))
        .await?;
    match tokio::time::timeout(Duration::from_secs(3), back_rx.recv()).await {
        Ok(Some(Reliable::Clipboard(t))) => {
            println!("REPLY {t}");
            if remote_wait.is_none() {
                ensure!(
                    t == format!("quickvm-e2e-reply-{nonce}"),
                    "回送內容不符：{t:?}"
                );
                println!("PASS 2 — Leave 回送內容正確");
            }
        }
        other => bail!("預期回送剪貼簿，得到 {other:?}"),
    }

    // 3. 去重：內容沒變再送 Leave，不應再回送。
    link.send_reliable(&Reliable::Control(Control::Leave))
        .await?;
    match tokio::time::timeout(Duration::from_secs(1), back_rx.recv()).await {
        Err(_) => println!("PASS 3 — 內容未變不重複回送（DEDUP-OK）"),
        Ok(other) => bail!("不該回送卻收到 {other:?}"),
    }

    println!("ALL PASS");
    Ok(())
}
