//! QUIC 傳輸：鍵盤 / 控制走 reliable uni stream、指標走 unreliable datagram。
//!
//! TODO(security): client 目前 skip-verify，改 SSH-like fingerprint 信任（首次比對 + 存檔）。
//! TODO(perf): CC 換 BBR / 調大 initial cwnd —— loss-based CC 在 WiFi 丟包會收縮、反增延遲。

use anyhow::Result;
use hmac::{Hmac, Mac};
use quickvm_proto::{
    decode_motion, decode_reliable, encode_motion, encode_reliable, Motion, Reliable,
};
use sha2::Sha256;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{ClientConfig, Connection, Endpoint, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;

const MAX_FRAME: usize = 64 * 1024;

/// 從對端收到的一則訊息。
#[derive(Debug)]
pub enum Incoming {
    Reliable(Reliable),
    Motion(Motion),
    /// 連線結束 —— 被控端據此 release 所有按住的鍵，避免主控斷線後黏鍵。
    Disconnected,
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
    // uni stream（鍵盤 / 按鈕 / 捲動 / 控制）—— 每則一條 stream。
    // accept_uni 出錯 = 連線結束 → 通知被控端 release 按住的鍵。
    tokio::spawn(async move {
        while let Ok(mut recv) = conn.accept_uni().await {
            if let Ok(buf) = recv.read_to_end(MAX_FRAME).await
                && let Ok(r) = decode_reliable(&buf)
            {
                let _ = tx.send(Incoming::Reliable(r)).await;
            }
        }
        let _ = tx.send(Incoming::Disconnected).await;
    });
}

/// 主控端：連到被控端，通過 PSK 認證後回傳連線句柄。
pub async fn connect(addr: SocketAddr, secret: Vec<u8>) -> Result<Link> {
    install_crypto();
    let mut ep = Endpoint::client("0.0.0.0:0".parse()?)?;
    ep.set_default_client_config(client_config()?);
    let conn = ep.connect(addr, "quickvm")?.await?;
    client_handshake(&conn, &secret).await?;
    Ok(Link {
        conn,
        motion_seq: 0,
    })
}

/// PSK challenge-response（HMAC-SHA256）。傳輸已 TLS 加密，但 skip-verify 不防 MITM，
/// 故 secret 不直接上線：serve 出 nonce、client 回 HMAC(secret, nonce)，serve 驗。
/// 擋掉「任何人連上就能注入鍵鼠」這個主威脅；MITM 重放被 per-連線新 nonce 擋掉。
const HS_NONCE_LEN: usize = 32;
const HS_MAC_LEN: usize = 32; // HMAC-SHA256 輸出長度

fn hmac_sha256(secret: &[u8], data: &[u8]) -> [u8; HS_MAC_LEN] {
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret).expect("HMAC 接受任意長度 key");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

async fn server_handshake(conn: &Connection, secret: &[u8]) -> Result<()> {
    use rand::RngCore;
    let mut nonce = [0u8; HS_NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce);
    let mut s = conn.open_uni().await?;
    s.write_all(&nonce).await?;
    s.finish()?;
    let mut r = conn.accept_uni().await?;
    let received = r.read_to_end(HS_MAC_LEN).await?;
    let mut mac = <Hmac<Sha256>>::new_from_slice(secret).expect("HMAC key");
    mac.update(&nonce);
    // verify_slice 是 constant-time 比較，避免 timing attack。
    mac.verify_slice(&received)
        .map_err(|_| anyhow::anyhow!("HMAC 不符 —— 共享密鑰不一致"))?;
    Ok(())
}

async fn client_handshake(conn: &Connection, secret: &[u8]) -> Result<()> {
    let mut r = conn.accept_uni().await?;
    let nonce = r.read_to_end(HS_NONCE_LEN).await?;
    let mac = hmac_sha256(secret, &nonce);
    let mut s = conn.open_uni().await?;
    s.write_all(&mac).await?;
    s.finish()?;
    Ok(())
}

/// 連線句柄：送可靠訊息 / 送指標 datagram。
pub struct Link {
    conn: Connection,
    motion_seq: u64,
}

impl Link {
    pub async fn send_reliable(&self, msg: &Reliable) -> Result<()> {
        let mut s = self.conn.open_uni().await?;
        s.write_all(&encode_reliable(msg)).await?;
        s.finish()?;
        Ok(())
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
