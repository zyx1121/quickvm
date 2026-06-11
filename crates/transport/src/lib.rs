//! QUIC 傳輸：鍵盤 / 控制走 reliable uni stream、指標走 unreliable datagram。
//!
//! TODO(security): client 目前 skip-verify，改 SSH-like fingerprint 信任（首次比對 + 存檔）。
//! TODO(perf): CC 換 BBR / 調大 initial cwnd —— loss-based CC 在 WiFi 丟包會收縮、反增延遲。

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use quickvm_proto::{
    FILES_MAX_TOTAL, FileMeta, FilesHeader, Motion, Reliable, STREAM_TAG_FILES, STREAM_TAG_MSG,
    decode_files_header, decode_motion, decode_reliable, encode_files_header, encode_motion,
    encode_reliable,
};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{ClientConfig, Connection, Endpoint, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::Sha256;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

// reliable frame 上限：要容得下剪貼簿訊息（CLIPBOARD_MAX + postcard enum/長度 overhead）。
const MAX_FRAME: usize = quickvm_proto::CLIPBOARD_MAX + 64;
// 檔案串流的讀寫 chunk。
const FILE_CHUNK: usize = 64 * 1024;

/// 從對端收到的一則訊息。
#[derive(Debug)]
pub enum Incoming {
    /// 主控端認證通過 —— 帶回送句柄（被控端 Leave 時回推剪貼簿用）。
    Connected(BackLink),
    Reliable(Reliable),
    /// 剪貼簿檔案：內容已 streaming 落地到本機暫存，這裡是落地後的路徑。
    Files(Vec<PathBuf>),
    Motion(Motion),
    /// 連線結束 —— 被控端據此 release 所有按住的鍵，避免主控斷線後黏鍵。
    Disconnected,
}

/// 被控端 → 主控端的回送句柄（uni stream，與主控端送來的方向相反）。
/// 可 clone：檔案回送 spawn 背景 task 用。
#[derive(Debug, Clone)]
pub struct BackLink {
    conn: Connection,
}

impl BackLink {
    pub async fn send_reliable(&self, msg: &Reliable) -> Result<()> {
        send_reliable_on(&self.conn, msg).await
    }

    pub async fn send_files(&self, paths: &[PathBuf]) -> Result<()> {
        send_files_on(&self.conn, paths).await
    }
}

async fn send_reliable_on(conn: &Connection, msg: &Reliable) -> Result<()> {
    let mut s = conn.open_uni().await?;
    s.write_all(&[STREAM_TAG_MSG]).await?;
    s.write_all(&encode_reliable(msg)).await?;
    s.finish()?;
    Ok(())
}

/// 檔案傳輸：tag + header(u32 LE 長度 + postcard) + 各檔 raw bytes 串接。
/// 邊讀邊送（不全載記憶體）；`take`-style 截斷保證檔案在傳輸中被改大時協定不錯位。
async fn send_files_on(conn: &Connection, paths: &[PathBuf]) -> Result<()> {
    let mut metas = Vec::with_capacity(paths.len());
    for p in paths {
        let md = tokio::fs::metadata(p)
            .await
            .with_context(|| format!("讀檔案 metadata 失敗：{}", p.display()))?;
        anyhow::ensure!(
            md.is_file(),
            "不是一般檔案（資料夾不支援）：{}",
            p.display()
        );
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .context("檔名非 UTF-8")?
            .to_string();
        metas.push(FileMeta {
            name,
            size: md.len(),
        });
    }
    let total: u64 = metas.iter().map(|m| m.size).sum();
    anyhow::ensure!(
        total <= FILES_MAX_TOTAL,
        "檔案總量 {total} 超過上限 {FILES_MAX_TOTAL}"
    );

    let mut s = conn.open_uni().await?;
    s.write_all(&[STREAM_TAG_FILES]).await?;
    let hdr = encode_files_header(&FilesHeader {
        files: metas.clone(),
    });
    s.write_all(&(hdr.len() as u32).to_le_bytes()).await?;
    s.write_all(&hdr).await?;

    let mut buf = vec![0u8; FILE_CHUNK];
    for (p, m) in paths.iter().zip(&metas) {
        let mut f = tokio::fs::File::open(p).await?;
        let mut remaining = m.size;
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            let n = f.read(&mut buf[..want]).await?;
            anyhow::ensure!(n > 0, "檔案在傳輸中縮小：{}", p.display());
            s.write_all(&buf[..n]).await?;
            remaining -= n as u64;
        }
    }
    s.finish()?;
    Ok(())
}

/// 接收檔案串流，落地到暫存目錄（每批一個子目錄，避免互相覆蓋），回傳落地路徑。
async fn recv_files(recv: &mut quinn::RecvStream) -> Result<Vec<PathBuf>> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let hlen = u32::from_le_bytes(len_buf) as usize;
    anyhow::ensure!(hlen <= MAX_FRAME, "files header 過大：{hlen}");
    let mut hbuf = vec![0u8; hlen];
    recv.read_exact(&mut hbuf).await?;
    let header = decode_files_header(&hbuf)?;
    let total: u64 = header.files.iter().map(|f| f.size).sum();
    anyhow::ensure!(total <= FILES_MAX_TOTAL, "檔案總量超過上限：{total}");

    let dir = new_recv_dir().await?;
    let mut out = Vec::with_capacity(header.files.len());
    let mut buf = vec![0u8; FILE_CHUNK];
    for meta in &header.files {
        let path = unique_path(&dir, sanitize_name(&meta.name));
        let mut f = tokio::fs::File::create(&path).await?;
        let mut remaining = meta.size;
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            let n = recv
                .read(&mut buf[..want])
                .await?
                .context("檔案串流提前結束")?;
            f.write_all(&buf[..n]).await?;
            remaining -= n as u64;
        }
        f.flush().await?;
        out.push(path);
    }
    Ok(out)
}

/// 暫存根目錄：`<tmp>/quickvm-files/<毫秒序號>/`。新批次開新子目錄；
/// 上限之前的舊批次保留（對端 clipboard 可能還指著），由啟動清理收走。
async fn new_recv_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("quickvm-files").join(format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
    ));
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

/// 啟動時清掉整個暫存根目錄 —— process 重啟後沒有任何 clipboard 引用仍然有效。
pub fn clean_recv_root() {
    let root = std::env::temp_dir().join("quickvm-files");
    if root.exists() {
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// 只取檔名本體，擋 path traversal（`..`、分隔符都進不來）。
fn sanitize_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unnamed");
    if base.is_empty() || base == ".." {
        "unnamed".to_string()
    } else {
        base.to_string()
    }
}

/// 同批重名時加 ` (n)` 後綴，不覆蓋。
fn unique_path(dir: &Path, name: String) -> PathBuf {
    let candidate = dir.join(&name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(&name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed");
    let ext = Path::new(&name).extension().and_then(|e| e.to_str());
    for i in 1.. {
        let alt = match ext {
            Some(e) => dir.join(format!("{stem} ({i}).{e}")),
            None => dir.join(format!("{stem} ({i})")),
        };
        if !alt.exists() {
            return alt;
        }
    }
    unreachable!()
}

/// rustls 0.23 需要 process-level crypto provider（冪等）。
fn install_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn server_config() -> Result<ServerConfig> {
    let cert = rcgen::generate_simple_self_signed(vec!["quickvm".to_string()])?;
    let cert_der = cert.cert.der().clone();
    let key_der = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    let mut sc = ServerConfig::with_single_cert(vec![cert_der], key_der.into())?;
    // 明設 idle timeout，跟 client keep-alive(5s) 對齊，不靠 quinn 預設。
    let mut tc = quinn::TransportConfig::default();
    tc.max_idle_timeout(Some(std::time::Duration::from_secs(30).try_into()?));
    sc.transport_config(Arc::new(tc));
    Ok(sc)
}

fn client_config() -> Result<ClientConfig> {
    let tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerify))
        .with_no_client_auth();
    let qcc = QuicClientConfig::try_from(Arc::new(tls))?;
    let mut cc = ClientConfig::new(Arc::new(qcc));
    // keep-alive：閒置時定期 PING 保活，否則使用者一段時間不動鍵鼠 → idle timeout 斷線。
    // 單端開即可（PING 往返重置雙方 idle timer）；interval 須 < server 預設 idle (~30s)。
    let mut tc = quinn::TransportConfig::default();
    tc.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    tc.max_idle_timeout(Some(std::time::Duration::from_secs(30).try_into()?));
    cc.transport_config(Arc::new(tc));
    Ok(cc)
}

/// 被控端：監聽，接受連線，PSK 認證通過後把收到的事件推進 channel。
pub async fn serve(bind: SocketAddr, secret: Vec<u8>) -> Result<mpsc::Receiver<Incoming>> {
    install_crypto();
    let ep = Endpoint::server(server_config()?, bind)?;
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let Ok(conn) = incoming.await else { continue };
            let tx = tx.clone();
            let secret = secret.clone();
            // 每條連線獨立認證，避免一個慢/惡意 handshake 卡住 accept loop。
            tokio::spawn(async move {
                match server_handshake(&conn, &secret).await {
                    Ok(()) => {
                        tracing::info!(peer = %conn.remote_address(), "controller authenticated");
                        let _ = tx
                            .send(Incoming::Connected(BackLink { conn: conn.clone() }))
                            .await;
                        spawn_recv(conn, tx);
                    }
                    Err(e) => {
                        tracing::warn!(peer = %conn.remote_address(), error = %e, "認證失敗，拒絕連線");
                        conn.close(1u32.into(), b"auth failed");
                    }
                }
            });
        }
    });
    Ok(rx)
}

fn spawn_recv(conn: Connection, tx: mpsc::Sender<Incoming>) {
    // datagram（指標移動）：丟棄過期 / 亂序封包（WiFi 上會亂序，舊封包會讓游標往回跳 → stutter）。
    let c = conn.clone();
    let t = tx.clone();
    tokio::spawn(async move {
        let mut last_seq = 0u64;
        while let Ok(b) = c.read_datagram().await {
            if let Ok(m) = decode_motion(&b) {
                if m.seq <= last_seq {
                    continue;
                }
                last_seq = m.seq;
                let _ = t.send(Incoming::Motion(m)).await;
            }
        }
    });
    // uni stream（小訊息 / 檔案串流，由 tag byte 分流）。
    // accept_uni 出錯 = 連線結束 → 通知被控端 release 按住的鍵。
    tokio::spawn(async move {
        while let Ok(recv) = conn.accept_uni().await {
            // 檔案串流可能跑很久，獨立 task 收 —— 不擋後面的鍵鼠訊息 stream。
            let tx = tx.clone();
            tokio::spawn(async move { handle_uni(recv, tx).await });
        }
        let _ = tx.send(Incoming::Disconnected).await;
    });
}

/// 讀 tag byte 分流一條 uni stream：小訊息（postcard）或檔案串流。
async fn handle_uni(mut recv: quinn::RecvStream, tx: mpsc::Sender<Incoming>) {
    let mut tag = [0u8; 1];
    if recv.read_exact(&mut tag).await.is_err() {
        return;
    }
    match tag[0] {
        STREAM_TAG_MSG => {
            if let Ok(buf) = recv.read_to_end(MAX_FRAME).await
                && let Ok(r) = decode_reliable(&buf)
            {
                let _ = tx.send(Incoming::Reliable(r)).await;
            }
        }
        STREAM_TAG_FILES => match recv_files(&mut recv).await {
            Ok(paths) => {
                let _ = tx.send(Incoming::Files(paths)).await;
            }
            Err(e) => tracing::warn!(error = %e, "接收剪貼簿檔案失敗"),
        },
        other => tracing::warn!(tag = other, "未知 stream tag，忽略"),
    }
}

/// 主控端：連到被控端，通過 PSK 認證後回傳連線句柄 + 對端回送訊息的 channel
/// （與 `Link` 分開回傳 —— 同一個 struct 在 select! 裡同時收又送會撞 borrow）。
/// 回送 channel 收 `Incoming::Reliable` / `Incoming::Files`（其餘 variant 不會出現）。
pub async fn connect(
    addr: SocketAddr,
    secret: Vec<u8>,
) -> Result<(Link, mpsc::Receiver<Incoming>)> {
    install_crypto();
    let mut ep = Endpoint::client("0.0.0.0:0".parse()?)?;
    ep.set_default_client_config(client_config()?);
    let conn = ep.connect(addr, "quickvm")?.await?;
    client_handshake(&conn, &secret).await?;
    // 收被控端的回送（剪貼簿回推）。連線死亡 accept_uni 出錯 → task 結束、channel 關閉。
    let (tx, rx) = mpsc::channel(16);
    let c = conn.clone();
    tokio::spawn(async move {
        while let Ok(recv) = c.accept_uni().await {
            let tx = tx.clone();
            tokio::spawn(async move { handle_uni(recv, tx).await });
        }
    });
    Ok((
        Link {
            conn,
            motion_seq: 0,
        },
        rx,
    ))
}

/// PSK challenge-response（HMAC-SHA256）。傳輸已 TLS 加密，但 skip-verify 不防 MITM，
/// 故 secret 不直接上線：serve 出 nonce、client 回 HMAC(secret, nonce)，serve 驗。
/// 擋掉「任何人連上就能注入鍵鼠」這個主威脅；MITM 重放被 per-連線新 nonce 擋掉。
const HS_VERSION: u8 = 3; // v3: uni stream 加 tag byte + 檔案串流頻道
const HS_NONCE_LEN: usize = 32;
const HS_MAC_LEN: usize = 32; // HMAC-SHA256 輸出長度

fn hmac_sha256(secret: &[u8], data: &[u8]) -> [u8; HS_MAC_LEN] {
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret).expect("HMAC 接受任意長度 key");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

// 用單一 bi stream（順序保證）：client 送版本 → serve 出 nonce → client 回 HMAC →
// serve 驗並回 1 byte verdict。client 必須等到 verdict 才算認證成功，否則錯 secret
// 會被 client 當成已連上、直到下次操作才斷。
async fn server_handshake(conn: &Connection, secret: &[u8]) -> Result<()> {
    use rand::RngCore;
    let (mut send, mut recv) = conn.accept_bi().await?;
    let mut ver = [0u8; 1];
    recv.read_exact(&mut ver).await?;
    if ver[0] != HS_VERSION {
        anyhow::bail!("協定版本不符（{} != {}）", ver[0], HS_VERSION);
    }
    let mut nonce = [0u8; HS_NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce);
    send.write_all(&nonce).await?;
    let mut received = [0u8; HS_MAC_LEN];
    recv.read_exact(&mut received).await?;
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret).expect("HMAC key");
    mac.update(&nonce);
    // verify_slice 是 constant-time 比較，避免 timing attack。
    let ok = mac.verify_slice(&received).is_ok();
    send.write_all(&[ok as u8]).await?;
    send.finish()?;
    if ok {
        Ok(())
    } else {
        anyhow::bail!("HMAC 不符 —— 共享密鑰不一致")
    }
}

async fn client_handshake(conn: &Connection, secret: &[u8]) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[HS_VERSION]).await?;
    let mut nonce = [0u8; HS_NONCE_LEN];
    recv.read_exact(&mut nonce).await?;
    let mac = hmac_sha256(secret, &nonce);
    send.write_all(&mac).await?;
    send.finish()?;
    let mut verdict = [0u8; 1];
    recv.read_exact(&mut verdict).await?;
    if verdict[0] == 1 {
        Ok(())
    } else {
        anyhow::bail!("認證被拒 —— 共享密鑰不符")
    }
}

/// 連線句柄：送可靠訊息 / 送指標 datagram。
pub struct Link {
    conn: Connection,
    motion_seq: u64,
}

/// 檔案傳輸句柄（`Link::files_link()` 取得），可 clone 進背景 task。
#[derive(Clone)]
pub struct FilesLink {
    conn: Connection,
}

impl FilesLink {
    pub async fn send_files(&self, paths: &[PathBuf]) -> Result<()> {
        send_files_on(&self.conn, paths).await
    }
}

impl Link {
    pub async fn send_reliable(&self, msg: &Reliable) -> Result<()> {
        send_reliable_on(&self.conn, msg).await
    }

    /// 可 clone 的檔案傳輸句柄 —— 檔案大、傳輸久，app 端 spawn 背景 task 用它送，
    /// 不借走 `Link` 本體（drive loop 還要繼續送鍵鼠）。
    pub fn files_link(&self) -> FilesLink {
        FilesLink {
            conn: self.conn.clone(),
        }
    }

    pub fn send_motion(&mut self, x: f64, y: f64) -> Result<()> {
        self.motion_seq += 1;
        let bytes = encode_motion(&Motion {
            seq: self.motion_seq,
            x,
            y,
        });
        self.conn.send_datagram(bytes::Bytes::from(bytes))?;
        Ok(())
    }

    /// 當前路徑的估計 round-trip 時間（診斷用）。
    pub fn rtt(&self) -> std::time::Duration {
        self.conn.rtt()
    }
}

#[derive(Debug)]
struct SkipVerify;

impl rustls::client::danger::ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ---- sanitize_name ----

    #[test]
    fn sanitize_keeps_plain_name() {
        assert_eq!(sanitize_name("report.txt"), "report.txt");
    }

    #[test]
    fn sanitize_strips_path_components() {
        // `/` 在所有平台都是分隔符，這兩個 case 平台無關。
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("a/b/c.txt"), "c.txt");
    }

    #[test]
    fn sanitize_backslash_safe_on_both_platforms() {
        // `\` 只有 Windows 當分隔符，Unix 是合法檔名字元 —— 期望值分平台，
        // 但安全不變量一致：結果是單一 component，join 後逃不出目的目錄。
        let got = sanitize_name("..\\windows\\evil.exe");
        if cfg!(windows) {
            assert_eq!(got, "evil.exe");
        } else {
            assert_eq!(got, "..\\windows\\evil.exe");
        }
        assert!(!got.contains('/'));
        assert_ne!(got, "..");
    }

    #[test]
    fn sanitize_degenerate_inputs_become_unnamed() {
        assert_eq!(sanitize_name(""), "unnamed");
        assert_eq!(sanitize_name(".."), "unnamed");
    }

    #[test]
    fn sanitize_preserves_unicode() {
        assert_eq!(sanitize_name("報告 🎉.txt"), "報告 🎉.txt");
    }

    // ---- unique_path ----

    /// 測試平行跑，共用目錄會互污衝突計數 —— 每測一個獨立子目錄，Drop 清理
    /// 確保 assert 失敗也不在 temp_dir 留垃圾。
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join("quickvm-transport-tests")
                .join(format!("{}-{tag}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn unique_path_returns_original_when_free() {
        let d = TestDir::new("free");
        assert_eq!(unique_path(&d.0, "a.txt".into()), d.0.join("a.txt"));
    }

    #[test]
    fn unique_path_suffixes_on_conflict() {
        let d = TestDir::new("conflict");
        fs::write(d.0.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_path(&d.0, "a.txt".into()), d.0.join("a (1).txt"));
    }

    #[test]
    fn unique_path_increments_until_free() {
        let d = TestDir::new("increment");
        fs::write(d.0.join("a.txt"), b"x").unwrap();
        fs::write(d.0.join("a (1).txt"), b"x").unwrap();
        assert_eq!(unique_path(&d.0, "a.txt".into()), d.0.join("a (2).txt"));
    }

    #[test]
    fn unique_path_conflict_without_extension() {
        let d = TestDir::new("noext");
        fs::write(d.0.join("data"), b"x").unwrap();
        assert_eq!(unique_path(&d.0, "data".into()), d.0.join("data (1)"));
    }
}
