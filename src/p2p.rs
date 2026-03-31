use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use bitcoin::Network;
use rand::random;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{lookup_host, TcpStream};
use tracing::{info, warn};

use crate::config::Config;
use crate::hash::double_sha256;
use crate::template::TemplateEngine;

const PROTOCOL_VERSION: i32 = 70016;
const MAX_PAYLOAD_LEN: usize = 4 * 1024 * 1024;
const CONNECT_TIMEOUT_SECS: u64 = 5;
const RECONNECT_DELAY_SECS: u64 = 3;
const STARTUP_PROBE_DELAY_SECS: u64 = 1;
const STARTUP_PROBE_TIMEOUT_SECS: u64 = 3;
const MSG_BLOCK: u32 = 2;
const MSG_FILTERED_BLOCK: u32 = 3;
const MSG_CMPCT_BLOCK: u32 = 4;
const MSG_WITNESS_FLAG: u32 = 1 << 30;

pub struct P2pClient {
    config: Config,
    template_engine: Arc<TemplateEngine>,
}

impl P2pClient {
    pub fn new(config: Config, template_engine: Arc<TemplateEngine>) -> Self {
        Self {
            config,
            template_engine,
        }
    }

    pub async fn run(self) {
        let Some(peer) = self.config.p2p_fast_peer.clone() else {
            return;
        };

        self.wait_for_startup_ready(&peer).await;

        loop {
            match self.run_session(&peer).await {
                Ok(()) => info!("p2p fast peer session ended peer={peer}"),
                Err(err) => warn!("p2p fast peer session failed peer={peer}: {err:?}"),
            }
            tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
        }
    }

    async fn wait_for_startup_ready(&self, peer: &str) {
        loop {
            match self.probe_peer_ready(peer).await {
                Ok(remote_addr) => {
                    info!("p2p fast peer startup ready peer={} resolved={}", peer, remote_addr);
                    return;
                }
                Err(err) => {
                    info!("p2p fast peer not ready yet peer={peer}: {err:?}");
                    tokio::time::sleep(Duration::from_secs(STARTUP_PROBE_DELAY_SECS)).await;
                }
            }
        }
    }

    async fn probe_peer_ready(&self, peer: &str) -> anyhow::Result<SocketAddr> {
        let (mut stream, remote_addr) = self.connect_peer(peer).await?;
        stream
            .set_nodelay(true)
            .context("enable tcp_nodelay for p2p startup probe")?;

        let version_payload = self
            .build_version_payload(remote_addr)
            .await
            .context("build version payload for startup probe")?;
        self.send_message(&mut stream, "version", &version_payload)
            .await
            .context("send version during startup probe")?;

        tokio::time::timeout(Duration::from_secs(STARTUP_PROBE_TIMEOUT_SECS), async {
            loop {
                let message = self.read_message(&mut stream).await?;
                match message.command.as_str() {
                    "version" => {
                        self.send_message(&mut stream, "verack", &[])
                            .await
                            .context("send verack during startup probe")?;
                        return Ok::<(), anyhow::Error>(());
                    }
                    "ping" => {
                        if message.payload.len() == 8 {
                            self.send_message(&mut stream, "pong", &message.payload)
                                .await
                                .context("send pong during startup probe")?;
                        }
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| anyhow!("startup probe timed out waiting for peer version"))??;

        Ok(remote_addr)
    }

    async fn run_session(&self, peer: &str) -> anyhow::Result<()> {
        let (mut stream, remote_addr) = self.connect_peer(peer).await?;
        stream
            .set_nodelay(true)
            .context("enable tcp_nodelay for p2p fast peer")?;

        let version_payload = self
            .build_version_payload(remote_addr)
            .await
            .context("build version payload")?;
        self.send_message(&mut stream, "version", &version_payload)
            .await
            .context("send version")?;

        let mut sent_verack = false;
        let mut received_verack = false;
        let mut sent_fast_handshake = false;

        loop {
            let message = self.read_message(&mut stream).await?;
            match message.command.as_str() {
                "version" => {
                    let peer_version = parse_peer_version(&message.payload).unwrap_or_default();
                    info!(
                        "p2p fast peer negotiated peer={} version={} start_height={}",
                        remote_addr, peer_version.version, peer_version.start_height
                    );
                    if !sent_verack {
                        self.send_message(&mut stream, "verack", &[])
                            .await
                            .context("send verack")?;
                        sent_verack = true;
                    }
                    if sent_verack && received_verack && !sent_fast_handshake {
                        self.finish_fast_handshake(&mut stream).await?;
                        sent_fast_handshake = true;
                    }
                }
                "verack" => {
                    received_verack = true;
                    if sent_verack && !sent_fast_handshake {
                        self.finish_fast_handshake(&mut stream).await?;
                        sent_fast_handshake = true;
                    }
                }
                "inv" => {
                    if !sent_fast_handshake {
                        continue;
                    }

                    let block_hashes = match parse_inv_block_hashes(&message.payload) {
                        Ok(hashes) => hashes,
                        Err(err) => {
                            warn!("p2p inv parse failed peer={remote_addr}: {err:?}");
                            continue;
                        }
                    };
                    if block_hashes.is_empty() {
                        continue;
                    }

                    let Some(locator_hash_le) = self.current_tip_locator_from_job().await else {
                        continue;
                    };
                    if let Err(err) = self.send_getheaders(&mut stream, locator_hash_le).await {
                        warn!("p2p getheaders send failed peer={remote_addr}: {err:?}");
                        continue;
                    }

                    info!(
                        "p2p fast inv fallback peer={} count={}",
                        remote_addr,
                        block_hashes.len()
                    );
                }
                "headers" => {
                    if !sent_fast_handshake {
                        continue;
                    }

                    let announcements = match parse_headers_announcements(&message.payload) {
                        Ok(announcements) => announcements,
                        Err(err) => {
                            warn!("p2p headers parse failed peer={remote_addr}: {err:?}");
                            continue;
                        }
                    };
                    if announcements.is_empty() {
                        continue;
                    }

                    let base_height = self.template_engine.current_job().await.height;
                    if base_height == 0 {
                        continue;
                    }

                    info!(
                        "p2p fast headers peer={} count={}",
                        remote_addr,
                        announcements.len()
                    );
                    for (idx, announcement) in announcements.iter().enumerate() {
                        let block_height = base_height.saturating_add(idx as u64);
                        if let Err(err) = self
                            .template_engine
                            .handle_fast_block_announcement(
                                &announcement.prevhash_hex,
                                &announcement.block_hash_hex,
                                announcement.block_time,
                                block_height,
                            )
                            .await
                        {
                            warn!(
                                "p2p fast headers apply failed peer={} hash={}: {err:?}",
                                remote_addr, announcement.block_hash_hex
                            );
                        }
                    }
                }
                "ping" => {
                    if message.payload.len() == 8 {
                        self.send_message(&mut stream, "pong", &message.payload)
                            .await
                            .context("send pong")?;
                    }
                }
                "cmpctblock" => {
                    if !sent_fast_handshake {
                        continue;
                    }

                    let block = match parse_cmpctblock_announcement(&message.payload) {
                        Ok(block) => block,
                        Err(err) => {
                            warn!("p2p cmpctblock parse failed peer={remote_addr}: {err:?}");
                            continue;
                        }
                    };

                    // BIP152 cmpctblock does not carry height; the current template height is the
                    // best local estimate for the announced block height.
                    let block_height = self.template_engine.current_job().await.height;
                    if block_height == 0 {
                        continue;
                    }

                    info!(
                        "p2p fast cmpctblock peer={} hash={}",
                        remote_addr, block.block_hash_hex
                    );
                    if let Err(err) = self
                        .template_engine
                        .handle_fast_block_announcement(
                            &block.prevhash_hex,
                            &block.block_hash_hex,
                            block.block_time,
                            block_height,
                        )
                        .await
                    {
                        warn!(
                            "p2p fast cmpctblock apply failed peer={} hash={}: {err:?}",
                            remote_addr, block.block_hash_hex
                        );
                    }
                }
                _ => {}
            }
        }
    }

    async fn connect_peer(&self, peer: &str) -> anyhow::Result<(TcpStream, SocketAddr)> {
        let addrs = resolve_peer_addrs(peer, default_port(self.config.network))
            .await
            .with_context(|| format!("resolve p2p fast peer {peer}"))?;

        let mut last_err: Option<anyhow::Error> = None;
        for addr in addrs {
            match tokio::time::timeout(
                Duration::from_secs(CONNECT_TIMEOUT_SECS),
                TcpStream::connect(addr),
            )
            .await
            {
                Ok(Ok(stream)) => {
                    info!("p2p fast peer connected peer={peer} resolved={addr}");
                    return Ok((stream, addr));
                }
                Ok(Err(err)) => {
                    last_err = Some(anyhow!("connect {addr} failed: {err}"));
                }
                Err(_) => {
                    last_err = Some(anyhow!("connect timeout for {addr}"));
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("no reachable addresses for {peer}")))
    }

    async fn build_version_payload(&self, remote_addr: SocketAddr) -> anyhow::Result<Vec<u8>> {
        let current_height = self.template_engine.current_job().await.height;
        let start_height = current_height.saturating_sub(1).min(i32::MAX as u64) as i32;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs() as i64;

        let mut payload = Vec::with_capacity(128);
        payload.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        payload.extend_from_slice(&0u64.to_le_bytes());
        payload.extend_from_slice(&now.to_le_bytes());
        payload.extend_from_slice(&encode_network_address(remote_addr.ip(), remote_addr.port()));
        payload.extend_from_slice(&encode_network_address(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            0,
        ));
        payload.extend_from_slice(&random::<u64>().to_le_bytes());

        let user_agent = format!("/stratumbee:{}/", env!("CARGO_PKG_VERSION"));
        encode_varint(user_agent.len() as u64, &mut payload);
        payload.extend_from_slice(user_agent.as_bytes());

        payload.extend_from_slice(&start_height.to_le_bytes());
        payload.push(0);
        Ok(payload)
    }

    async fn finish_fast_handshake(&self, stream: &mut TcpStream) -> anyhow::Result<()> {
        self.send_message(stream, "sendheaders", &[])
            .await
            .context("send sendheaders")?;
        self.send_compact_block_mode(stream, 2).await?;
        self.send_compact_block_mode(stream, 1).await
    }

    async fn current_tip_locator_from_job(&self) -> Option<[u8; 32]> {
        let current = self.template_engine.current_job().await;
        if !current.ready || current.height == 0 {
            None
        } else {
            Some(current.prevhash_le_bytes)
        }
    }

    async fn send_compact_block_mode(
        &self,
        stream: &mut TcpStream,
        version: u64,
    ) -> anyhow::Result<()> {
        let payload = build_sendcmpct_payload(version);
        self.send_message(stream, "sendcmpct", &payload)
            .await
            .context("send sendcmpct")
    }

    async fn send_getheaders(
        &self,
        stream: &mut TcpStream,
        locator_hash_le: [u8; 32],
    ) -> anyhow::Result<()> {
        let payload = build_getheaders_payload(locator_hash_le);
        self.send_message(stream, "getheaders", &payload)
            .await
            .context("send getheaders")
    }

    async fn send_message(
        &self,
        stream: &mut TcpStream,
        command: &str,
        payload: &[u8],
    ) -> anyhow::Result<()> {
        if command.len() > 12 {
            return Err(anyhow!("p2p command too long: {command}"));
        }
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(anyhow!("p2p payload too large for {command}"));
        }

        let mut header = [0u8; 24];
        header[..4].copy_from_slice(&network_magic(self.config.network));
        header[4..4 + command.len()].copy_from_slice(command.as_bytes());
        header[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        header[20..24].copy_from_slice(&payload_checksum(payload));

        stream.write_all(&header).await?;
        stream.write_all(payload).await?;
        stream.flush().await?;
        Ok(())
    }

    async fn read_message(&self, stream: &mut TcpStream) -> anyhow::Result<MessageFrame> {
        let mut header = [0u8; 24];
        stream.read_exact(&mut header).await?;

        let magic = network_magic(self.config.network);
        if &header[..4] != &magic[..] {
            return Err(anyhow!(
                "unexpected p2p magic: expected={} got={}",
                hex::encode(magic),
                hex::encode(&header[..4])
            ));
        }

        let command_end = header[4..16]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(12);
        let command = String::from_utf8_lossy(&header[4..4 + command_end]).to_string();
        let payload_len = u32::from_le_bytes(
            header[16..20]
                .try_into()
                .expect("p2p header payload length slice must be 4 bytes"),
        ) as usize;
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(anyhow!(
                "p2p payload too large for {command}: {payload_len} bytes"
            ));
        }

        let mut payload = vec![0u8; payload_len];
        stream.read_exact(&mut payload).await?;

        let checksum = payload_checksum(&payload);
        if &header[20..24] != &checksum[..] {
            return Err(anyhow!("bad p2p checksum for {command}"));
        }

        Ok(MessageFrame { command, payload })
    }
}

struct MessageFrame {
    command: String,
    payload: Vec<u8>,
}

#[derive(Default)]
struct PeerVersion {
    version: i32,
    start_height: i32,
}

struct BlockAnnouncement {
    prevhash_hex: String,
    block_hash_hex: String,
    block_time: u32,
}

async fn resolve_peer_addrs(peer: &str, default_port: u16) -> anyhow::Result<Vec<SocketAddr>> {
    let mut attempts = vec![peer.to_string()];
    if !peer.contains(':') {
        attempts.push(format!("{peer}:{default_port}"));
    } else if peer.matches(':').count() > 1 && !peer.starts_with('[') {
        attempts.push(format!("[{peer}]:{default_port}"));
    }

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in attempts {
        match lookup_host(attempt.as_str()).await {
            Ok(addrs) => {
                let resolved = addrs.collect::<Vec<_>>();
                if !resolved.is_empty() {
                    return Ok(resolved);
                }
            }
            Err(err) => {
                last_err = Some(anyhow!("lookup {attempt} failed: {err}"));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("failed to resolve {peer}")))
}

fn parse_peer_version(payload: &[u8]) -> anyhow::Result<PeerVersion> {
    if payload.len() < 80 {
        return Err(anyhow!("version payload too short"));
    }

    let version = i32::from_le_bytes(
        payload[0..4]
            .try_into()
            .expect("version field must be 4 bytes"),
    );

    let mut pos = 80usize;
    let user_agent_len = read_varint(payload, &mut pos)? as usize;
    if pos + user_agent_len > payload.len() {
        return Err(anyhow!("version user_agent exceeds payload"));
    }
    pos += user_agent_len;

    let start_height = if pos + 4 <= payload.len() {
        i32::from_le_bytes(
            payload[pos..pos + 4]
                .try_into()
                .expect("start_height field must be 4 bytes"),
        )
    } else {
        0
    };

    Ok(PeerVersion {
        version,
        start_height,
    })
}

fn build_sendcmpct_payload(version: u64) -> Vec<u8> {
    let mut payload = Vec::with_capacity(9);
    payload.push(1);
    payload.extend_from_slice(&version.to_le_bytes());
    payload
}

fn parse_inv_block_hashes(payload: &[u8]) -> anyhow::Result<Vec<[u8; 32]>> {
    let mut pos = 0usize;
    let count = usize::try_from(read_varint(payload, &mut pos)?)
        .map_err(|_| anyhow!("inv count does not fit in usize"))?;
    let mut hashes = Vec::with_capacity(count.min(64));

    for _ in 0..count {
        let inv_type = u32::from_le_bytes(
            payload
                .get(pos..pos + 4)
                .ok_or_else(|| anyhow!("inv type truncated"))?
                .try_into()
                .expect("inv type must be 4 bytes"),
        );
        pos += 4;

        let hash_bytes: [u8; 32] = payload
            .get(pos..pos + 32)
            .ok_or_else(|| anyhow!("inv hash truncated"))?
            .try_into()
            .expect("inv hash must be 32 bytes");
        pos += 32;

        if is_block_inventory_type(inv_type) {
            hashes.push(hash_bytes);
        }
    }

    if pos != payload.len() {
        return Err(anyhow!("inv payload has trailing bytes"));
    }

    Ok(hashes)
}

fn build_getheaders_payload(locator_hash_le: [u8; 32]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + 1 + 32 + 32);
    payload.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    encode_varint(1, &mut payload);
    payload.extend_from_slice(&locator_hash_le);
    payload.extend_from_slice(&[0u8; 32]);
    payload
}

fn parse_headers_announcements(payload: &[u8]) -> anyhow::Result<Vec<BlockAnnouncement>> {
    let mut pos = 0usize;
    let count = usize::try_from(read_varint(payload, &mut pos)?)
        .map_err(|_| anyhow!("headers count does not fit in usize"))?;
    let mut announcements = Vec::with_capacity(count.min(32));

    for _ in 0..count {
        let header = payload
            .get(pos..pos + 80)
            .ok_or_else(|| anyhow!("headers payload truncated"))?;
        pos += 80;
        announcements.push(parse_block_announcement_from_header(header)?);

        let txn_count = read_varint(payload, &mut pos)?;
        if txn_count != 0 {
            return Err(anyhow!("headers transaction count must be zero"));
        }
    }

    if pos != payload.len() {
        return Err(anyhow!("headers payload has trailing bytes"));
    }

    Ok(announcements)
}

fn parse_cmpctblock_announcement(payload: &[u8]) -> anyhow::Result<BlockAnnouncement> {
    if payload.len() < 90 {
        return Err(anyhow!("cmpctblock payload too short"));
    }

    parse_block_announcement_from_header(&payload[..80])
}

fn parse_block_announcement_from_header(header: &[u8]) -> anyhow::Result<BlockAnnouncement> {
    if header.len() != 80 {
        return Err(anyhow!("block header must be 80 bytes"));
    }

    let prevhash_hex = {
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&header[4..36]);
        bytes.reverse();
        hex::encode(bytes)
    };

    let block_hash_hex = {
        let mut hash = double_sha256(header);
        hash.reverse();
        hex::encode(hash)
    };

    let block_time = u32::from_le_bytes(
        header[68..72]
            .try_into()
            .expect("block time field must be 4 bytes"),
    );

    Ok(BlockAnnouncement {
        prevhash_hex,
        block_hash_hex,
        block_time,
    })
}

fn is_block_inventory_type(inv_type: u32) -> bool {
    matches!(
        inv_type & !MSG_WITNESS_FLAG,
        MSG_BLOCK | MSG_FILTERED_BLOCK | MSG_CMPCT_BLOCK
    )
}

fn network_magic(network: Network) -> [u8; 4] {
    match network {
        Network::Bitcoin => [0xf9, 0xbe, 0xb4, 0xd9],
        Network::Testnet => [0x0b, 0x11, 0x09, 0x07],
        Network::Signet => [0x0a, 0x03, 0xcf, 0x40],
        Network::Regtest => [0xfa, 0xbf, 0xb5, 0xda],
        _ => [0x0b, 0x11, 0x09, 0x07],
    }
}

fn default_port(network: Network) -> u16 {
    match network {
        Network::Bitcoin => 8333,
        Network::Testnet => 18333,
        Network::Signet => 38333,
        Network::Regtest => 18444,
        _ => 18333,
    }
}

fn encode_network_address(ip: IpAddr, port: u16) -> [u8; 26] {
    let mut out = [0u8; 26];
    out[..8].copy_from_slice(&0u64.to_le_bytes());

    match ip {
        IpAddr::V4(ipv4) => {
            out[18] = 0xff;
            out[19] = 0xff;
            out[20..24].copy_from_slice(&ipv4.octets());
        }
        IpAddr::V6(ipv6) => {
            out[8..24].copy_from_slice(&ipv6.octets());
        }
    }

    out[24..26].copy_from_slice(&port.to_be_bytes());
    out
}

fn payload_checksum(payload: &[u8]) -> [u8; 4] {
    let hash = double_sha256(payload);
    [hash[0], hash[1], hash[2], hash[3]]
}

fn encode_varint(value: u64, out: &mut Vec<u8>) {
    match value {
        0..=0xfc => out.push(value as u8),
        0xfd..=0xffff => {
            out.push(0xfd);
            out.extend_from_slice(&(value as u16).to_le_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(0xfe);
            out.extend_from_slice(&(value as u32).to_le_bytes());
        }
        _ => {
            out.push(0xff);
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

fn read_varint(payload: &[u8], pos: &mut usize) -> anyhow::Result<u64> {
    let prefix = *payload
        .get(*pos)
        .ok_or_else(|| anyhow!("varint prefix missing"))?;
    *pos += 1;

    match prefix {
        0x00..=0xfc => Ok(prefix as u64),
        0xfd => {
            let bytes = payload
                .get(*pos..*pos + 2)
                .ok_or_else(|| anyhow!("varint u16 truncated"))?;
            *pos += 2;
            Ok(u16::from_le_bytes(
                bytes.try_into().expect("u16 varint must be 2 bytes"),
            ) as u64)
        }
        0xfe => {
            let bytes = payload
                .get(*pos..*pos + 4)
                .ok_or_else(|| anyhow!("varint u32 truncated"))?;
            *pos += 4;
            Ok(u32::from_le_bytes(
                bytes.try_into().expect("u32 varint must be 4 bytes"),
            ) as u64)
        }
        0xff => {
            let bytes = payload
                .get(*pos..*pos + 8)
                .ok_or_else(|| anyhow!("varint u64 truncated"))?;
            *pos += 8;
            Ok(u64::from_le_bytes(
                bytes.try_into().expect("u64 varint must be 8 bytes"),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_getheaders_payload, build_sendcmpct_payload, encode_network_address,
        parse_cmpctblock_announcement, parse_headers_announcements, parse_inv_block_hashes,
        read_varint,
    };
    use crate::hash::double_sha256;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn encode_network_address_maps_ipv4_into_ipv6_space() {
        let encoded = encode_network_address(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8333);

        assert_eq!(&encoded[8..20], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff]);
        assert_eq!(&encoded[20..24], &[1, 2, 3, 4]);
        assert_eq!(&encoded[24..26], &8333u16.to_be_bytes());
    }

    #[test]
    fn encode_network_address_keeps_ipv6_octets() {
        let ip = "2001:db8::5".parse::<Ipv6Addr>().unwrap();
        let encoded = encode_network_address(IpAddr::V6(ip), 18444);

        assert_eq!(&encoded[8..24], &ip.octets());
        assert_eq!(&encoded[24..26], &18444u16.to_be_bytes());
    }

    #[test]
    fn parse_cmpctblock_announcement_reads_prevhash_hash_and_time() {
        let mut header = [0u8; 80];
        header[0..4].copy_from_slice(&0x2000_0000u32.to_le_bytes());
        for idx in 0..32 {
            header[4 + idx] = (idx + 1) as u8;
        }
        header[68..72].copy_from_slice(&1_700_000_000u32.to_le_bytes());

        let mut payload = header.to_vec();
        payload.extend_from_slice(&123u64.to_le_bytes());
        payload.push(0);
        payload.push(0);

        let announcement = parse_cmpctblock_announcement(&payload).unwrap();

        let mut expected_prevhash = [0u8; 32];
        expected_prevhash.copy_from_slice(&header[4..36]);
        expected_prevhash.reverse();
        assert_eq!(announcement.prevhash_hex, hex::encode(expected_prevhash));

        let mut expected_hash = double_sha256(&header);
        expected_hash.reverse();
        assert_eq!(announcement.block_hash_hex, hex::encode(expected_hash));
        assert_eq!(announcement.block_time, 1_700_000_000);
    }

    #[test]
    fn build_sendcmpct_payload_sets_announce_mode_and_version() {
        let payload = build_sendcmpct_payload(2);
        let mut pos = 0usize;

        let announce = payload[pos];
        pos += 1;
        let version = u64::from_le_bytes(payload[pos..pos + 8].try_into().unwrap());
        pos += 8;

        assert_eq!(announce, 1);
        assert_eq!(version, 2);
        assert_eq!(pos, payload.len());
    }

    #[test]
    fn parse_inv_block_hashes_filters_block_inventory() {
        let mut payload = Vec::new();
        super::encode_varint(3, &mut payload);

        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&[0x11; 32]);

        payload.extend_from_slice(&2u32.to_le_bytes());
        payload.extend_from_slice(&[0x22; 32]);

        payload.extend_from_slice(&(super::MSG_WITNESS_FLAG | 2).to_le_bytes());
        payload.extend_from_slice(&[0x33; 32]);

        let hashes = parse_inv_block_hashes(&payload).unwrap();

        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes[0], [0x22; 32]);
        assert_eq!(hashes[1], [0x33; 32]);
    }

    #[test]
    fn build_getheaders_payload_encodes_single_locator() {
        let locator = [0x55; 32];

        let payload = build_getheaders_payload(locator);
        let mut pos = 0usize;

        let version = i32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap());
        pos += 4;
        let locator_count = read_varint(&payload, &mut pos).unwrap();
        let locator_hash: [u8; 32] = payload[pos..pos + 32].try_into().unwrap();
        pos += 32;
        let stop_hash: [u8; 32] = payload[pos..pos + 32].try_into().unwrap();
        pos += 32;

        assert_eq!(version, super::PROTOCOL_VERSION);
        assert_eq!(locator_count, 1);
        assert_eq!(locator_hash, locator);
        assert_eq!(stop_hash, [0u8; 32]);
        assert_eq!(pos, payload.len());
    }

    #[test]
    fn parse_headers_announcements_reads_prevhash_hash_and_time() {
        let mut header = [0u8; 80];
        header[0..4].copy_from_slice(&0x2000_0000u32.to_le_bytes());
        for idx in 0..32 {
            header[4 + idx] = (idx + 1) as u8;
        }
        header[68..72].copy_from_slice(&1_700_000_001u32.to_le_bytes());

        let mut payload = Vec::new();
        super::encode_varint(1, &mut payload);
        payload.extend_from_slice(&header);
        payload.push(0);

        let announcements = parse_headers_announcements(&payload).unwrap();
        let announcement = &announcements[0];

        let mut expected_prevhash = [0u8; 32];
        expected_prevhash.copy_from_slice(&header[4..36]);
        expected_prevhash.reverse();
        assert_eq!(announcement.prevhash_hex, hex::encode(expected_prevhash));

        let mut expected_hash = double_sha256(&header);
        expected_hash.reverse();
        assert_eq!(announcement.block_hash_hex, hex::encode(expected_hash));
        assert_eq!(announcement.block_time, 1_700_000_001);
    }

    #[test]
    fn read_varint_decodes_fd_prefixed_values() {
        let payload = [0xfd, 0x34, 0x12];
        let mut pos = 0usize;

        let value = read_varint(&payload, &mut pos).unwrap();

        assert_eq!(value, 0x1234);
        assert_eq!(pos, payload.len());
    }
}
