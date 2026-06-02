use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use binary_sv2::{Deserialize, GetSize, Serialize};
use codec_sv2::{
    Error as CodecError, HandshakeRole, NoiseEncoder, StandardEitherFrame, StandardNoiseDecoder,
    StandardSv2Frame, State,
};
use framing_sv2::framing::Sv2Frame;
use rand::RngCore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::config::Config;
use crate::metrics::MetricsStore;
use crate::template::TemplateEngine;

use super::connection::Sv2Connection;
use super::keys::AuthorityKeyPair;

#[derive(Clone)]
pub struct Sv2Server {
    config: Config,
    template_engine: Arc<TemplateEngine>,
    metrics: MetricsStore,
    authority: AuthorityKeyPair,
}

impl Sv2Server {
    pub fn new(
        config: Config,
        template_engine: Arc<TemplateEngine>,
        metrics: MetricsStore,
    ) -> anyhow::Result<Self> {
        let authority = AuthorityKeyPair::load(&config)?;
        Ok(Self {
            config,
            template_engine,
            metrics,
            authority,
        })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let listener = TcpListener::bind((self.config.sv2_bind.as_str(), self.config.sv2_port))
            .await
            .with_context(|| {
                format!(
                    "bind sv2 listener {}:{}",
                    self.config.sv2_bind, self.config.sv2_port
                )
            })?;
        info!(
            "sv2 listener started addr={}:{}",
            self.config.sv2_bind, self.config.sv2_port
        );

        let connection_limiter = Arc::new(Semaphore::new(self.config.sv2_max_connections));
        let handshake_limiter = Arc::new(Semaphore::new(self.config.sv2_max_handshakes));
        let ip_limiter = Arc::new(IpConnectionLimiter::new(
            self.config.sv2_max_connections_per_ip,
        ));

        loop {
            let (stream, peer_addr) = listener.accept().await.context("accept sv2 connection")?;
            let Ok(connection_permit) = connection_limiter.clone().try_acquire_owned() else {
                self.metrics.counters.inc_sv2_rejected_max_connections();
                warn!(%peer_addr, "sv2 connection rejected: max connections reached");
                continue;
            };
            let Some(ip_permit) = ip_limiter.try_acquire(peer_addr.ip()) else {
                self.metrics.counters.inc_sv2_rejected_per_ip();
                warn!(%peer_addr, "sv2 connection rejected: per-ip connection limit reached");
                continue;
            };

            let handshake_limiter = handshake_limiter.clone();
            let authority = self.authority.clone();
            let config = self.config.clone();
            let template_engine = self.template_engine.clone();
            let metrics = self.metrics.clone();

            tokio::spawn(async move {
                let _connection_permit = connection_permit;
                let _ip_permit = ip_permit;
                metrics.counters.inc_sv2_connection_opened();
                let result = async {
                    let _handshake_permit = handshake_limiter
                        .acquire_owned()
                        .await
                        .context("acquire sv2 handshake permit")?;
                    let transport = match Sv2Transport::accept(stream, authority, &config).await {
                        Ok(transport) => {
                            metrics.counters.inc_sv2_handshake_success();
                            transport
                        }
                        Err(err) => {
                            metrics.counters.inc_sv2_handshake_failure();
                            return Err(err)
                                .with_context(|| format!("sv2 noise handshake from {peer_addr}"));
                        }
                    };
                    drop(_handshake_permit);

                    Sv2Connection::new(
                        config,
                        template_engine,
                        metrics.clone(),
                        transport,
                        peer_addr,
                    )
                    .run()
                    .await
                }
                .await;

                metrics.counters.dec_sv2_connection_active();
                metrics.counters.inc_sv2_connection_closed();
                if let Err(err) = result {
                    warn!(%peer_addr, "sv2 connection closed: {err:#}");
                }
            });
        }
    }
}

struct IpConnectionLimiter {
    max_per_ip: usize,
    counts: Mutex<HashMap<IpAddr, usize>>,
}

impl IpConnectionLimiter {
    fn new(max_per_ip: usize) -> Self {
        Self {
            max_per_ip,
            counts: Mutex::new(HashMap::new()),
        }
    }

    fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<IpConnectionPermit> {
        let mut counts = self.counts.lock().expect("sv2 ip limiter mutex poisoned");
        let count = counts.entry(ip).or_insert(0);
        if *count >= self.max_per_ip {
            return None;
        }
        *count += 1;
        Some(IpConnectionPermit {
            limiter: self.clone(),
            ip,
        })
    }

    fn release(&self, ip: IpAddr) {
        let mut counts = self.counts.lock().expect("sv2 ip limiter mutex poisoned");
        if let Some(count) = counts.get_mut(&ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&ip);
            }
        }
    }
}

struct IpConnectionPermit {
    limiter: Arc<IpConnectionLimiter>,
    ip: IpAddr,
}

impl Drop for IpConnectionPermit {
    fn drop(&mut self) {
        self.limiter.release(self.ip);
    }
}

#[derive(Debug)]
pub struct RawMessage {
    payload: Vec<u8>,
}

impl RawMessage {
    fn new(payload: Vec<u8>) -> Self {
        Self { payload }
    }
}

impl GetSize for RawMessage {
    fn get_size(&self) -> usize {
        self.payload.len()
    }
}

impl Serialize for RawMessage {
    fn to_bytes(self, dst: &mut [u8]) -> Result<usize, binary_sv2::Error> {
        if dst.len() < self.payload.len() {
            return Err(binary_sv2::Error::WriteError(self.payload.len(), dst.len()));
        }
        dst[..self.payload.len()].copy_from_slice(&self.payload);
        Ok(self.payload.len())
    }
}

impl<'a> Deserialize<'a> for RawMessage {
    fn get_structure(
        _data: &[u8],
    ) -> Result<Vec<binary_sv2::decodable::FieldMarker>, binary_sv2::Error> {
        Ok(Vec::new())
    }

    fn from_decoded_fields(
        _data: Vec<binary_sv2::decodable::DecodableField<'a>>,
    ) -> Result<Self, binary_sv2::Error> {
        Ok(Self {
            payload: Vec::new(),
        })
    }

    fn from_bytes(data: &'a mut [u8]) -> Result<Self, binary_sv2::Error> {
        Ok(Self {
            payload: data.to_vec(),
        })
    }
}

#[derive(Debug)]
pub struct Sv2RawFrame {
    pub msg_type: u8,
    pub extension_type: u16,
    pub channel_msg: bool,
    pub payload: Vec<u8>,
}

pub struct Sv2Transport {
    stream: TcpStream,
    state: State,
    decoder: StandardNoiseDecoder<RawMessage>,
    encoder: NoiseEncoder<RawMessage>,
    frame_max_bytes: usize,
    idle_timeout: Duration,
}

pub struct Sv2Reader {
    stream: OwnedReadHalf,
    state: State,
    decoder: StandardNoiseDecoder<RawMessage>,
    frame_max_bytes: usize,
    idle_timeout: Duration,
}

pub struct Sv2Writer {
    stream: OwnedWriteHalf,
    state: State,
    encoder: NoiseEncoder<RawMessage>,
    frame_max_bytes: usize,
}

impl Sv2Transport {
    pub async fn accept(
        mut stream: TcpStream,
        authority: AuthorityKeyPair,
        config: &Config,
    ) -> anyhow::Result<Self> {
        let responder = authority.responder()?;
        let mut state = State::initialized(HandshakeRole::Responder(responder));

        let mut first_message = [0u8; noise_sv2::ELLSWIFT_ENCODING_SIZE];
        tokio::time::timeout(
            Duration::from_secs(config.sv2_handshake_timeout_secs),
            stream.read_exact(&mut first_message),
        )
        .await
        .context("sv2 handshake timed out waiting for initiator")?
        .context("read sv2 initiator handshake")?;

        let (second_message, transport_state) = state
            .step_1(first_message)
            .map_err(|err| anyhow!("sv2 responder handshake step_1: {err:?}"))?;
        let second_payload = second_message.get_payload_when_handshaking();
        stream
            .write_all(&second_payload)
            .await
            .context("write sv2 responder handshake")?;

        Ok(Self {
            stream,
            state: transport_state,
            decoder: StandardNoiseDecoder::new(),
            encoder: NoiseEncoder::new(),
            frame_max_bytes: config.sv2_frame_max_bytes,
            idle_timeout: Duration::from_secs(config.sv2_idle_timeout_secs),
        })
    }

    pub fn split(self) -> (Sv2Reader, Sv2Writer) {
        let read_state = self.state.clone();
        let write_state = self.state;
        let (read_half, write_half) = self.stream.into_split();
        (
            Sv2Reader {
                stream: read_half,
                state: read_state,
                decoder: self.decoder,
                frame_max_bytes: self.frame_max_bytes,
                idle_timeout: self.idle_timeout,
            },
            Sv2Writer {
                stream: write_half,
                state: write_state,
                encoder: self.encoder,
                frame_max_bytes: self.frame_max_bytes,
            },
        )
    }
}

impl Sv2Reader {
    pub async fn read_frame(&mut self) -> anyhow::Result<Sv2RawFrame> {
        loop {
            let writable = self.decoder.writable();
            if !writable.is_empty() {
                tokio::time::timeout(self.idle_timeout, self.stream.read_exact(writable))
                    .await
                    .context("sv2 idle timeout while reading frame")?
                    .context("read sv2 frame")?;
            }

            match self.decoder.next_frame(&mut self.state) {
                Ok(frame) => {
                    let mut frame: StandardSv2Frame<RawMessage> = frame
                        .try_into()
                        .map_err(|err| anyhow!("decode sv2 frame: {err:?}"))?;
                    let header = frame
                        .get_header()
                        .ok_or_else(|| anyhow!("decoded sv2 frame missing header"))?;
                    let payload = frame.payload().to_vec();
                    if payload.len() > self.frame_max_bytes {
                        bail!(
                            "sv2 frame payload too large: {} > {}",
                            payload.len(),
                            self.frame_max_bytes
                        );
                    }
                    return Ok(Sv2RawFrame {
                        msg_type: header.msg_type(),
                        extension_type: header.ext_type_without_channel_msg(),
                        channel_msg: header.channel_msg(),
                        payload,
                    });
                }
                Err(CodecError::MissingBytes(missing)) => {
                    let encrypted_limit = self.frame_max_bytes.saturating_add(4096);
                    if missing > encrypted_limit {
                        bail!(
                            "sv2 encrypted frame too large: missing {} bytes exceeds limit {}",
                            missing,
                            encrypted_limit
                        );
                    }
                }
                Err(err) => return Err(anyhow!("decode sv2 frame: {err:?}")),
            }
        }
    }
}

impl Sv2Writer {
    pub async fn send_message<T>(
        &mut self,
        message: T,
        msg_type: u8,
        channel_msg: bool,
    ) -> anyhow::Result<()>
    where
        T: Serialize + GetSize,
    {
        let payload = binary_sv2::to_bytes(message)
            .map_err(|err| anyhow!("serialize sv2 message: {err:?}"))?;
        if payload.len() > self.frame_max_bytes {
            bail!(
                "sv2 outbound frame payload too large: {} > {}",
                payload.len(),
                self.frame_max_bytes
            );
        }

        let frame = Sv2Frame::from_message(RawMessage::new(payload), msg_type, 0, channel_msg)
            .ok_or_else(|| anyhow!("build sv2 frame type={msg_type}"))?;
        let frame = StandardEitherFrame::<RawMessage>::Sv2(frame);
        let encoded = self
            .encoder
            .encode(frame, &mut self.state)
            .map_err(|err| anyhow!("encode sv2 frame type={msg_type}: {err:?}"))?;
        self.stream
            .write_all(encoded.as_ref())
            .await
            .with_context(|| format!("write sv2 frame type={msg_type}"))?;
        Ok(())
    }
}

pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}
