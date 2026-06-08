//! QUIC 傳輸：鍵盤 / 控制走 reliable uni stream、指標走 unreliable datagram。
//!
//! TODO(security): client 目前 skip-verify，改 SSH-like fingerprint 信任（首次比對 + 存檔）。
//! TODO(perf): CC 換 BBR / 調大 initial cwnd —— loss-based CC 在 WiFi 丟包會收縮、反增延遲。

use anyhow::Result;
use quickvm_proto::{
    decode_motion, decode_reliable, encode_motion, encode_reliable, Motion, Reliable,
};
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
}

/// rustls 0.23 需要 process-level crypto provider（冪等）。
fn install_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn server_config() -> Result<ServerConfig> {
    let cert = rcgen::generate_simple_self_signed(vec!["quickvm".to_string()])?;
    let cert_der = cert.cert.der().clone();
    let key_der = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    Ok(ServerConfig::with_single_cert(vec![cert_der], key_der.into())?)
}

fn client_config() -> Result<ClientConfig> {
    let tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerify))
        .with_no_client_auth();
    let qcc = QuicClientConfig::try_from(Arc::new(tls))?;
    Ok(ClientConfig::new(Arc::new(qcc)))
}

/// 被控端：監聽，接受連線，把收到的事件推進 channel。
pub async fn serve(bind: SocketAddr) -> Result<mpsc::Receiver<Incoming>> {
    install_crypto();
    let ep = Endpoint::server(server_config()?, bind)?;
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let Ok(conn) = incoming.await else { continue };
            tracing::info!(peer = %conn.remote_address(), "controller connected");
            spawn_recv(conn, tx.clone());
        }
    });
    Ok(rx)
}

fn spawn_recv(conn: Connection, tx: mpsc::Sender<Incoming>) {
    // datagram（指標移動）
    let c = conn.clone();
    let t = tx.clone();
    tokio::spawn(async move {
        while let Ok(b) = c.read_datagram().await {
            if let Ok(m) = decode_motion(&b) {
                let _ = t.send(Incoming::Motion(m)).await;
            }
        }
    });
    // uni stream（鍵盤 / 按鈕 / 捲動 / 控制）—— 每則一條 stream
    tokio::spawn(async move {
        while let Ok(mut recv) = conn.accept_uni().await {
            if let Ok(buf) = recv.read_to_end(MAX_FRAME).await {
                if let Ok(r) = decode_reliable(&buf) {
                    let _ = tx.send(Incoming::Reliable(r)).await;
                }
            }
        }
    });
}

/// 主控端：連到被控端。
pub async fn connect(addr: SocketAddr) -> Result<Link> {
    install_crypto();
    let mut ep = Endpoint::client("0.0.0.0:0".parse()?)?;
    ep.set_default_client_config(client_config()?);
    let conn = ep.connect(addr, "quickvm")?.await?;
    Ok(Link {
        conn,
        motion_seq: 0,
    })
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
