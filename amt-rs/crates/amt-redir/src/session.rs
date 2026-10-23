//! Intel AMT redirection transport: TLS, framing and the AAAH handshake.
//!
//! Protocol reference: MeshCommander's `amt-redir-ws` (Apache-2.0), extracted
//! in this repository under `ref/`. The handshake is:
//!
//!   1. connect TLS to 16994 and send the protocol magic (SOL/KVM/IDER)
//!   2. receive 0x11 StartRedirectionSessionReply, status 0
//!   3. send 0x13 with authType 0 (query supported methods)
//!   4. receive 0x14 with authType 0, list of methods: we need 4 (Digest+qop)
//!   5. send 0x13 with authType 4, user and authuri
//!   6. receive 0x14 with authType 4: realm, nonce, qop
//!   7. send 0x13 with authType 4 and the MD5 digest
//!   8. receive 0x14 with status 0 (authenticated)
//!   9. protocol-specific start: SOL 0x20, KVM 0x40, IDER nothing
//!  10. SOL: receive 0x21 (settings) and acknowledge with 0x27

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rand::Rng;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsStream;

use crate::digest::intel_digest;

fn trace(msg: &str) {
    if std::env::var("AMT_TRACE").is_ok() {
        eprintln!("[amt] {msg}");
    }
}

pub const AUTH_URI: &str = "/RedirectionService";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Sol,
    Kvm,
    Ider,
}

impl Protocol {
    fn magic(self) -> &'static [u8] {
        match self {
            Protocol::Sol => &[0x10, 0x00, 0x00, 0x00, b'S', b'O', b'L', b' '],
            Protocol::Kvm => &[0x10, 0x01, 0x00, 0x00, b'K', b'V', b'M', b'R'],
            Protocol::Ider => &[0x10, 0x00, 0x00, 0x00, b'I', b'D', b'E', b'R'],
        }
    }
}

/// AMT uses a self-signed certificate on the redirection port and the host
/// address is an IP, so there is nothing sensible to verify against.
#[derive(Debug)]
struct AcceptAny(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub struct Session {
    stream: TlsStream<TcpStream>,
    seq: u32,
    pub protocol: Protocol,
    /// Serial settings reported by the device, once the SOL session is up.
    pub serial_settings: Option<Vec<u8>>,
}

impl Session {
    pub async fn connect(host: &str, port: u16, protocol: Protocol) -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        // Intel AMT's redirection service is happiest with TLS 1.2; with 1.3
        // the handshake completes but the service then drops the connection.
        let config = ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS12])
            .context("TLS protocol versions")?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
            .with_no_client_auth();

        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tcp = TcpStream::connect((host, port))
            .await
            .with_context(|| format!("TCP connect to {}:{}", host, port))?;
        let _ = tcp.set_nodelay(true);

        // Use the IP as the server name: rustls then sends no SNI at all.
        // Intel AMT closes the connection when a SNI it does not know arrives.
        let server_name = match host.parse::<std::net::IpAddr>() {
            Ok(ip) => ServerName::IpAddress(ip.into()),
            Err(_) => ServerName::try_from(host.to_string())
                .context("server name")?
                .to_owned(),
        };
        let stream = connector
            .connect(server_name, tcp)
            .await
            .context("TLS handshake with the redirection port")?;

        Ok(Self {
            stream: tokio_rustls::TlsStream::Client(stream),
            seq: 0,
            protocol,
            serial_settings: None,
        })
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        self.stream.write_all(data).await.context("write")?;
        self.stream.flush().await.context("flush")?;
        Ok(())
    }

    /// Raw access to the stream, for protocols carried inside the redirection
    /// session (RFB for KVM, the IDER protocol, ...).
    pub async fn read_raw(&mut self, n: usize) -> Result<Vec<u8>> {
        self.read_n(n).await
    }

    pub async fn write_raw(&mut self, data: &[u8]) -> Result<()> {
        self.send(data).await
    }

    async fn read_n(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        self.stream.read_exact(&mut buf).await.context("read")?;
        Ok(buf)
    }

    /// Read one redirection frame: returns (type, payload-without-type-byte).
    pub async fn read_frame(&mut self) -> Result<(u8, Vec<u8>)> {
        let ty = self.read_n(1).await?[0];
        let payload = match ty {
            0x11 => {
                // status at [1], oem length at [12]
                let rest = self.read_n(12).await?;
                let oem_len = rest[11] as usize;
                if oem_len > 0 {
                    let mut v = rest;
                    v.extend_from_slice(&self.read_n(oem_len).await?);
                    v
                } else {
                    rest
                }
            }
            0x14 => {
                let rest = self.read_n(8).await?;
                let len = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
                let mut v = rest;
                if len > 0 {
                    v.extend_from_slice(&self.read_n(len).await?);
                }
                v
            }
            0x21 => self.read_n(22).await?,
            0x29 => self.read_n(9).await?,
            0x2A => {
                let rest = self.read_n(9).await?;
                let len = u16::from_le_bytes([rest[7], rest[8]]) as usize;
                let mut v = rest;
                if len > 0 {
                    v.extend_from_slice(&self.read_n(len).await?);
                }
                v
            }
            0x2B => self.read_n(7).await?,
            0x41 => self.read_n(7).await?,
            0xF0 => Vec::new(),
            other => bail!("unexpected redirection frame type 0x{:02x}", other),
        };
        Ok((ty, payload))
    }

    pub async fn handshake(&mut self, user: &str, pass: &str) -> Result<()> {
        trace("tls established, sending protocol magic");
        self.send(self.protocol.magic()).await?;
        trace("magic sent, waiting for 0x11");

        let (ty, p) = self.read_frame().await?;
        if ty != 0x11 {
            bail!("expected StartRedirectionSessionReply (0x11), got 0x{:02x}", ty);
        }
        if p[0] != 0 {
            bail!("StartRedirectionSession refused, status {}", p[0]);
        }

        // Ask which authentication methods the device supports.
        trace("got 0x11, querying auth methods");
        self.send(&[0x13, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
            .await?;
        let (ty, p) = self.read_frame().await?;
        if ty != 0x14 {
            bail!("expected AuthenticateSessionReply (0x14), got 0x{:02x}", ty);
        }
        trace("got auth methods reply");
        let auth_type = p[3];
        let methods = &p[8..];
        if auth_type != 0 {
            bail!("expected an auth-methods reply, got authType {}", auth_type);
        }
        if !methods.contains(&4) {
            bail!(
                "device does not offer Digest with qop (authType 4); methods: {:?}",
                methods
            );
        }

        // Select Digest (authType 4) and tell it who we are.
        let mut msg = Vec::new();
        msg.extend_from_slice(&[0x13, 0x00, 0x00, 0x00, 0x04]);
        msg.extend_from_slice(&((user.len() + AUTH_URI.len() + 8) as u32).to_le_bytes());
        msg.push(user.len() as u8);
        msg.extend_from_slice(user.as_bytes());
        msg.extend_from_slice(&[0x00, 0x00]);
        msg.push(AUTH_URI.len() as u8);
        msg.extend_from_slice(AUTH_URI.as_bytes());
        msg.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        self.send(&msg).await?;

        // Challenge: realm, nonce, qop.
        let (ty, p) = self.read_frame().await?;
        if ty != 0x14 {
            bail!("expected the Digest challenge, got 0x{:02x}", ty);
        }
        trace("got challenge");
        let status = p[0];
        let auth_type = p[3];
        if status != 1 || auth_type != 4 {
            bail!(
                "unexpected Digest challenge: status {} authType {}",
                status,
                auth_type
            );
        }
        let data = &p[8..];
        let mut i = 0usize;
        let realm_len = *data.get(i).context("challenge truncated (realm)")? as usize;
        i += 1;
        let realm = String::from_utf8_lossy(&data[i..i + realm_len]).to_string();
        i += realm_len;
        let nonce_len = *data.get(i).context("challenge truncated (nonce)")? as usize;
        i += 1;
        let nonce = String::from_utf8_lossy(&data[i..i + nonce_len]).to_string();
        i += nonce_len;
        let qop_len = *data.get(i).context("challenge truncated (qop)")? as usize;
        i += 1;
        let qop = String::from_utf8_lossy(&data[i..i + qop_len]).to_string();

        let cnonce: String = {
            let mut rng = rand::thread_rng();
            (0..32)
                .map(|_| {
                    let n: u8 = rng.gen_range(0..16);
                    char::from_digit(n as u32, 16).unwrap()
                })
                .collect()
        };
        let snc = "00000002";
        let digest = intel_digest(
            user,
            &realm,
            pass,
            &nonce,
            AUTH_URI,
            &cnonce,
            snc,
            Some(&qop),
        );

        let mut msg = Vec::new();
        let total = user.len()
            + realm.len()
            + nonce.len()
            + AUTH_URI.len()
            + cnonce.len()
            + snc.len()
            + digest.len()
            + 7
            + qop.len()
            + 1;
        msg.extend_from_slice(&[0x13, 0x00, 0x00, 0x00, 0x04]);
        msg.extend_from_slice(&(total as u32).to_le_bytes());
        for field in [
            user.as_bytes(),
            realm.as_bytes(),
            nonce.as_bytes(),
            AUTH_URI.as_bytes(),
            cnonce.as_bytes(),
            snc.as_bytes(),
            digest.as_bytes(),
            qop.as_bytes(),
        ] {
            msg.push(field.len() as u8);
            msg.extend_from_slice(field);
        }
        self.send(&msg).await?;

        trace("digest sent, waiting for the result");
        let (ty, p) = self.read_frame().await?;
        if ty != 0x14 {
            bail!("expected the authentication result, got 0x{:02x}", ty);
        }
        if p[0] != 0 {
            bail!(
                "authentication failed, status {} (authType {})",
                p[0],
                p.get(3).copied().unwrap_or(0)
            );
        }

        trace("authenticated");
        // Protocol start.
        match self.protocol {
            Protocol::Sol => {
                // Serial settings: max tx 10000, timeouts as MeshCommander sends.
                let mut msg = vec![0x20, 0x00, 0x00, 0x00];
                msg.extend_from_slice(&self.seq.to_le_bytes());
                self.seq += 1;
                msg.extend_from_slice(&10000u16.to_le_bytes()); // max tx buffer
                msg.extend_from_slice(&100u16.to_le_bytes()); // tx timeout
                msg.extend_from_slice(&0u16.to_le_bytes()); // tx overflow timeout
                msg.extend_from_slice(&10000u16.to_le_bytes()); // rx timeout
                msg.extend_from_slice(&100u16.to_le_bytes()); // rx flush timeout
                msg.extend_from_slice(&0u16.to_le_bytes()); // heartbeat
                msg.extend_from_slice(&0u32.to_le_bytes());
                self.send(&msg).await?;

                // 0x21 then 0x29 (serial settings).
                loop {
                    let (ty, p) = self.read_frame().await?;
                    match ty {
                        0x21 => {
                            let mut ack = vec![0x27, 0x00, 0x00, 0x00];
                            ack.extend_from_slice(&self.seq.to_le_bytes());
                            self.seq += 1;
                            ack.extend_from_slice(&[0x00, 0x00, 0x1B, 0x00, 0x00, 0x00]);
                            self.send(&ack).await?;
                        }
                        0x29 => {
                            self.serial_settings = Some(p.clone());
                            break;
                        }
                        0x2B => {
                            // keepalive: answer in kind
                            let mut ka = vec![0x2B, 0x00, 0x00, 0x00];
                            ka.extend_from_slice(&self.seq.to_le_bytes());
                            self.seq += 1;
                            self.send(&ka).await?;
                        }
                        other => bail!("unexpected frame 0x{:02x} during SOL start", other),
                    }
                }
            }
            Protocol::Kvm => {
                self.send(&[0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
                    .await?;
                let (ty, _) = self.read_frame().await?;
                if ty != 0x41 {
                    bail!("expected KVM start ack (0x41), got 0x{:02x}", ty);
                }
            }
            Protocol::Ider => {}
        }

        Ok(())
    }

    /// Send data over SOL (frame 0x28).
    pub async fn sol_send(&mut self, data: &[u8]) -> Result<()> {
        let mut msg = vec![0x28, 0x00, 0x00, 0x00];
        msg.extend_from_slice(&self.seq.to_le_bytes());
        self.seq += 1;
        msg.extend_from_slice(&(data.len() as u16).to_le_bytes());
        msg.extend_from_slice(data);
        self.send(&msg).await
    }

    /// Read one frame and return the SOL payload, if it carries any.
    pub async fn sol_read(&mut self) -> Result<Option<Vec<u8>>> {
        let (ty, p) = self.read_frame().await?;
        match ty {
            0x2A => Ok(Some(p[9..].to_vec())),
            0x2B => {
                let mut ka = vec![0x2B, 0x00, 0x00, 0x00];
                ka.extend_from_slice(&self.seq.to_le_bytes());
                self.seq += 1;
                self.send(&ka).await?;
                Ok(None)
            }
            _ => Ok(None),
        }
    }
}
