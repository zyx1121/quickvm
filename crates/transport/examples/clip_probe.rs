//! 剪貼簿同步 E2E probe —— 對著本機跑的真 `quickvm serve` 驗整條鏈路：
//! 推送寫入、Leave 回送、去重不重送。probe 與 serve 同機，看的是同一個系統剪貼簿。
//!
//! 用法（兩個終端，同一個 QUICKVM_SECRET）：
//!   QUICKVM_SECRET=… quickvm serve --bind 127.0.0.1:7777
//!   QUICKVM_SECRET=… cargo run -p quickvm-transport --example clip_probe

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
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:7777".into())
        .parse()?;
    let secret = std::env::var("QUICKVM_SECRET")
        .context("需要 QUICKVM_SECRET（跟 serve 同值）")?
        .into_bytes();
    let (link, mut back_rx) = quickvm_transport::connect(addr, secret).await?;
    let nonce = std::process::id();

    // 1. controller→serve 推送：serve 應把內容寫進（本機）剪貼簿。
    let push = format!("quickvm-e2e-push-{nonce}");
    link.send_reliable(&Reliable::Clipboard(push.clone()))
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let got = pbpaste()?;
    ensure!(got == push, "推送後 pbpaste = {got:?}，預期 {push:?}");
    println!("PASS 1 — 推送已寫入被控端剪貼簿");

    // 2. serve→controller 回送：改剪貼簿後送 Leave，應回送新內容。
    let reply = format!("quickvm-e2e-reply-{nonce}");
    pbcopy(&reply)?;
    link.send_reliable(&Reliable::Control(Control::Leave))
        .await?;
    match tokio::time::timeout(Duration::from_secs(3), back_rx.recv()).await {
        Ok(Some(Reliable::Clipboard(t))) if t == reply => {
            println!("PASS 2 — Leave 回送內容正確")
        }
        other => bail!("預期回送 {reply:?}，得到 {other:?}"),
    }

    // 3. 去重：內容沒變再送 Leave，不應再回送。
    link.send_reliable(&Reliable::Control(Control::Leave))
        .await?;
    match tokio::time::timeout(Duration::from_secs(1), back_rx.recv()).await {
        Err(_) => println!("PASS 3 — 內容未變不重複回送"),
        Ok(other) => bail!("不該回送卻收到 {other:?}"),
    }

    println!("ALL PASS");
    Ok(())
}
