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
        Ok(Some(quickvm_transport::Incoming::Reliable(Reliable::Clipboard(t)))) => {
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

    if remote_wait.is_none() {
        run_file_tests(&link, &mut back_rx, nonce).await?;
    }

    println!("ALL PASS");
    Ok(())
}

/// 檔案剪貼簿鏈路（本機模式）：推送落地 + 內容一致、回送方向、檔名 sanitize。
async fn run_file_tests(
    link: &quickvm_transport::Link,
    back_rx: &mut tokio::sync::mpsc::Receiver<quickvm_transport::Incoming>,
    nonce: u32,
) -> Result<()> {
    use quickvm_transport::Incoming;

    // 4. 檔案推送：兩個檔（中文檔名 + 二進位內容）→ serve 應落地暫存並設剪貼簿。
    let src_dir = std::env::temp_dir().join(format!("quickvm-probe-{nonce}"));
    std::fs::create_dir_all(&src_dir)?;
    let payload_a: Vec<u8> = (0..1_000_000u32).flat_map(|i| i.to_le_bytes()).collect();
    let payload_b = "中文內容 🦀\r\n".repeat(1000).into_bytes();
    let file_a = src_dir.join("資料 a.bin");
    let file_b = src_dir.join("b.txt");
    std::fs::write(&file_a, &payload_a)?;
    std::fs::write(&file_b, &payload_b)?;

    let before = recv_dirs()?;
    link.files_link()
        .send_files(&[file_a.clone(), file_b.clone()])
        .await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let new_dir = recv_dirs()?
        .into_iter()
        .find(|d| !before.contains(d))
        .context("serve 沒有落地新暫存目錄")?;
    let landed_a = std::fs::read(new_dir.join("資料 a.bin"))?;
    let landed_b = std::fs::read(new_dir.join("b.txt"))?;
    ensure!(landed_a == payload_a, "檔案 a 內容不一致");
    ensure!(landed_b == payload_b, "檔案 b 內容不一致");
    println!(
        "PASS 4 — 檔案推送落地，內容 byte-level 一致（{} bytes）",
        payload_a.len() + payload_b.len()
    );

    // 5. 檔案回送：把本機剪貼簿設成另一個檔案（osascript），Leave 應回送它。
    let reply_file = src_dir.join("reply.bin");
    let reply_payload: Vec<u8> = (0..200_000u32)
        .flat_map(|i| (i ^ 0xA5A5).to_le_bytes())
        .collect();
    std::fs::write(&reply_file, &reply_payload)?;
    let script = format!(
        "set the clipboard to (POSIX file \"{}\")",
        reply_file.display()
    );
    let st = Command::new("osascript").args(["-e", &script]).status()?;
    ensure!(st.success(), "osascript 設定檔案剪貼簿失敗");
    link.send_reliable(&Reliable::Control(Control::Leave))
        .await?;
    match tokio::time::timeout(Duration::from_secs(5), back_rx.recv()).await {
        Ok(Some(Incoming::Files(paths))) => {
            ensure!(paths.len() == 1, "預期回送 1 個檔案，得到 {}", paths.len());
            let got = std::fs::read(&paths[0])?;
            ensure!(got == reply_payload, "回送檔案內容不一致");
            println!("PASS 5 — Leave 回送檔案，內容一致（{} bytes）", got.len());
        }
        other => bail!("預期回送檔案，得到 {other:?}"),
    }

    // 6. 檔案去重：剪貼簿沒變再 Leave，不應重複回送。
    link.send_reliable(&Reliable::Control(Control::Leave))
        .await?;
    match tokio::time::timeout(Duration::from_secs(1), back_rx.recv()).await {
        Err(_) => println!("PASS 6 — 檔案未變不重複回送（DEDUP-OK）"),
        Ok(other) => bail!("不該回送卻收到 {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&src_dir);
    Ok(())
}

/// serve 落地用的暫存子目錄列表（probe 與 serve 同機才有意義）。
fn recv_dirs() -> Result<Vec<std::path::PathBuf>> {
    let root = std::env::temp_dir().join("quickvm-files");
    if !root.exists() {
        return Ok(vec![]);
    }
    Ok(std::fs::read_dir(root)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect())
}
