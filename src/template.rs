use std::str::FromStr;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use bitcoin::Address;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{watch, Mutex, RwLock, Semaphore};
use tracing::{info, warn};

use crate::config::Config;
use crate::hash::{double_sha256, merkle_step};
use crate::metrics::PoolCounters;
use crate::rpc::RpcClient;

const NO_PENDING_REFRESH_SOURCE: u8 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TemplateSource {
    Startup = 1,
    Longpoll = 2,
    LongpollTimeoutFallback = 3,
    LongpollErrorFallback = 4,
    ZmqHashblock = 5,
    ZmqRawblock = 6,
    P2pCmpctFast = 7,
}

impl TemplateSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Longpoll => "longpoll",
            Self::LongpollTimeoutFallback => "longpoll_timeout_fallback",
            Self::LongpollErrorFallback => "longpoll_error_fallback",
            Self::ZmqHashblock => "zmq_hashblock",
            Self::ZmqRawblock => "zmq_rawblock",
            Self::P2pCmpctFast => "p2p_cmpct_fast",
        }
    }

    const fn pending_slot(self) -> u8 {
        self as u8
    }

    const fn from_pending_slot(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Startup),
            2 => Some(Self::Longpoll),
            3 => Some(Self::LongpollTimeoutFallback),
            4 => Some(Self::LongpollErrorFallback),
            5 => Some(Self::ZmqHashblock),
            6 => Some(Self::ZmqRawblock),
            7 => Some(Self::P2pCmpctFast),
            _ => None,
        }
    }
}

impl std::fmt::Display for TemplateSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct JobTemplate {
    pub ready: bool,
    pub job_id: String,
    pub submit_old: bool,
    pub prevhash: String,
    pub prevhash_le: String,
    pub coinbase1: String,
    pub coinbase2: String,
    pub merkle_branches: Vec<String>,
    pub version: String,
    pub nbits: String,
    pub ntime: String,
    pub height: u64,
    pub transactions: Vec<String>,
    pub has_witness_commitment: bool,
    pub coinbase_value: u64,
    pub created_at: DateTime<Utc>,
    pub prevhash_le_bytes: [u8; 32],
    pub target_le: [u8; 32],
    pub coinbase1_bytes: Vec<u8>,
    pub coinbase2_bytes: Vec<u8>,
    pub merkle_branches_le: Vec<[u8; 32]>,
    pub version_u32: u32,
    pub nbits_u32: u32,
    pub mintime_u32: u32,
    pub network_difficulty: f64,
    pub template_key: String,
    pub template_source: TemplateSource,
    pub txid_partial_root: String,
    pub witness_commitment_script: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitBlockOutcome {
    Submitted,
    Duplicate,
}

impl JobTemplate {
    pub fn empty() -> Self {
        Self {
            ready: false,
            job_id: "0".to_string(),
            submit_old: true,
            prevhash: String::new(),
            prevhash_le: String::new(),
            coinbase1: String::new(),
            coinbase2: String::new(),
            merkle_branches: vec![],
            version: String::new(),
            nbits: String::new(),
            ntime: String::new(),
            height: 0,
            transactions: vec![],
            has_witness_commitment: false,
            coinbase_value: 0,
            created_at: Utc::now(),
            prevhash_le_bytes: [0u8; 32],
            target_le: [0u8; 32],
            coinbase1_bytes: vec![],
            coinbase2_bytes: vec![],
            merkle_branches_le: vec![],
            version_u32: 0,
            nbits_u32: 0,
            mintime_u32: 0,
            network_difficulty: 0.0,
            template_key: String::new(),
            template_source: TemplateSource::Startup,
            txid_partial_root: String::new(),
            witness_commitment_script: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GbtResponse {
    longpollid: Option<String>,
    submitold: Option<bool>,
    version: u32,
    previousblockhash: String,
    transactions: Vec<GbtTx>,
    coinbasevalue: u64,
    bits: String,
    curtime: u64,
    mintime: Option<u64>,
    height: u64,
    target: String,
    coinbaseaux: Option<GbtCoinbaseAux>,
    default_witness_commitment: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GbtCoinbaseAux {
    flags: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GbtTx {
    data: String,
    txid: Option<String>,
    hash: Option<String>,
}

#[derive(Debug, Clone)]
struct ChainTipState {
    hash_hex: String,
    height: u64,
    median_time_past: u32,
    nbits: String,
    nbits_u32: u32,
    target_le: [u8; 32],
    version_u32: u32,
    network_difficulty: f64,
}

#[derive(Clone)]
pub struct TemplateEngine {
    config: Config,
    rpc: RpcClient,
    sender: watch::Sender<Arc<JobTemplate>>,
    current: Arc<RwLock<Arc<JobTemplate>>>,
    chain_tip: Arc<RwLock<Option<ChainTipState>>>,
    job_counter: Arc<AtomicU64>,
    last_template_key: Arc<Mutex<Option<String>>>,
    refresh_sem: Arc<Semaphore>,
    refresh_pending_source: Arc<AtomicU8>,
    last_zmq_block_trigger_ms: Arc<AtomicU64>,
    last_longpollid: Arc<Mutex<Option<String>>>,
    counters: Arc<PoolCounters>,
}

impl TemplateEngine {
    pub fn new(config: Config, rpc: RpcClient, counters: Arc<PoolCounters>) -> Self {
        let empty = Arc::new(JobTemplate::empty());
        let (sender, _) = watch::channel(empty.clone());
        Self {
            config,
            rpc,
            sender,
            current: Arc::new(RwLock::new(empty)),
            chain_tip: Arc::new(RwLock::new(None)),
            job_counter: Arc::new(AtomicU64::new(1)),
            last_template_key: Arc::new(Mutex::new(None)),
            refresh_sem: Arc::new(Semaphore::new(1)),
            refresh_pending_source: Arc::new(AtomicU8::new(NO_PENDING_REFRESH_SOURCE)),
            last_zmq_block_trigger_ms: Arc::new(AtomicU64::new(0)),
            last_longpollid: Arc::new(Mutex::new(None)),
            counters,
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<JobTemplate>> {
        self.sender.subscribe()
    }

    pub async fn current_job(&self) -> Arc<JobTemplate> {
        self.current.read().await.clone()
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        self.refresh_template(TemplateSource::Startup)
            .await
            .context("initial template refresh failed")?;

        let longpoll_engine = self.clone();
        tokio::spawn(async move {
            longpoll_engine.longpoll_loop().await;
        });

        if !self.config.zmq_block_urls.is_empty() {
            let zmq_engine = self.clone();
            let urls = self.config.zmq_block_urls.clone();
            tokio::spawn(async move {
                zmq_engine.zmq_block_loop(urls).await;
            });
        }

        info!(
            "template engine started mode={} periodic_gbt_polling=disabled",
            if self.config.zmq_block_urls.is_empty() {
                "longpoll"
            } else {
                "longpoll+zmq"
            }
        );

        Ok(())
    }

    pub async fn handle_fast_block_announcement(
        &self,
        prevhash_hex: &str,
        block_hash_hex: &str,
        block_time: u32,
        block_height: u64,
    ) -> anyhow::Result<()> {
        let permit = match self.refresh_sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };

        let tip = {
            let guard = self.chain_tip.read().await;
            guard.clone()
        };
        let tip = match tip {
            Some(tip) => tip,
            None => {
                drop(permit);
                return Ok(());
            }
        };

        if !tip.hash_hex.eq_ignore_ascii_case(prevhash_hex) || block_height != tip.height + 1 {
            drop(permit);
            return Ok(());
        }

        let next_height = block_height.saturating_add(1);
        if self.config.network == bitcoin::Network::Bitcoin && next_height % 2016 == 0 {
            drop(permit);
            return Ok(());
        }

        let block_hash_bytes =
            hex::decode(block_hash_hex).context("decode fast block announcement hash")?;
        let next_prevhash_le = to_little_endian(&block_hash_bytes)?;
        let current = self.current_job().await;
        if current.ready && current.height == next_height && current.prevhash_le == next_prevhash_le {
            drop(permit);
            return Ok(());
        }
        if current.ready && current.height > next_height {
            drop(permit);
            return Ok(());
        }

        let estimated_mtp = tip.median_time_past.max(block_time);
        let key = format!(
            "fast:{}:{}:{}:{}",
            block_hash_hex,
            next_height,
            tip.nbits,
            tip.version_u32
        );

        let key_guard = self.last_template_key.lock().await;
        if key_guard.as_deref() == Some(&key) {
            self.counters.inc_notify_deduped();
            drop(key_guard);
            drop(permit);
            return Ok(());
        }
        drop(key_guard);

        let job = self.build_empty_job(&tip, block_hash_hex, next_height, estimated_mtp, key.clone())?;
        info!(
            "new template source={} height={} txs={} reward={} sat nbits={}",
            job.template_source,
            job.height,
            job.transactions.len(),
            job.coinbase_value,
            job.nbits
        );

        *self.chain_tip.write().await = Some(ChainTipState {
            hash_hex: block_hash_hex.to_string(),
            height: block_height,
            median_time_past: estimated_mtp,
            nbits: tip.nbits.clone(),
            nbits_u32: tip.nbits_u32,
            target_le: tip.target_le,
            version_u32: tip.version_u32,
            network_difficulty: tip.network_difficulty,
        });
        self.publish_job(job, key).await;

        drop(permit);
        Ok(())
    }

    pub async fn submit_block(
        &self,
        block_hex: &str,
        block_hash_hex: &str,
        template_key: &str,
        coinbase_hex: &str,
        txid_partial_root: &str,
        witness_script: &str,
    ) -> anyhow::Result<SubmitBlockOutcome> {
        let submit_delays_ms = [0u64, 500, 1000, 2000, 4000];
        let mut last_submit_err: Option<anyhow::Error> = None;

        for (attempt, delay_ms) in submit_delays_ms.into_iter().enumerate() {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            match self
                .rpc
                .call_optional::<String>("submitblock", json!([block_hex]))
                .await
            {
                Ok(None) => {
                    self.counters.inc_submitblock_accepted();
                    last_submit_err = None;
                    break;
                }
                Ok(Some(reason)) => {
                    if reason == "duplicate" {
                        self.counters.inc_submitblock_accepted();
                        warn!("submitblock duplicate for {block_hash_hex}; treating as accepted");
                        return Ok(SubmitBlockOutcome::Duplicate);
                    }

                    self.counters.inc_submitblock_rejected();
                    let category = categorise_reject_reason(&reason);
                    let header_hex = if block_hex.len() >= 160 {
                        &block_hex[..160]
                    } else {
                        block_hex
                    };
                    tracing::error!(
                        "SUBMITBLOCK REJECTED reason=\"{}\" category={} hash={} template_key=\"{}\"",
                        reason,
                        category,
                        block_hash_hex,
                        template_key
                    );
                    tracing::error!("  header_hex={header_hex}");
                    tracing::error!("  coinbase_hex={coinbase_hex}");
                    tracing::error!("  txid_partial_root={txid_partial_root}");
                    if !witness_script.is_empty() {
                        tracing::error!("  witness_commitment_script={witness_script}");
                    }
                    return Err(anyhow!("submitblock rejected: {reason}"));
                }
                Err(err) => {
                    tracing::error!(
                        "submitblock RPC attempt {}/{} failed: {err:?}",
                        attempt + 1,
                        submit_delays_ms.len()
                    );
                    last_submit_err = Some(err);
                }
            }
        }

        if let Some(err) = last_submit_err {
            self.counters.inc_submitblock_rpc_fail();
            return Err(err).context("submitblock RPC failed after all retries; BLOCK MAY BE LOST");
        }

        for (idx, delay_ms) in [300u64, 600, 1200, 2400, 4800].into_iter().enumerate() {
            let header_res: anyhow::Result<serde_json::Value> = self
                .rpc
                .call("getblockheader", json!([block_hash_hex, false]))
                .await;
            match header_res {
                Ok(_) => {
                    tracing::info!("BLOCK CONFIRMED in node: {block_hash_hex}");
                    return Ok(SubmitBlockOutcome::Submitted);
                }
                Err(err) => {
                    let msg = err.to_string();
                    if !msg.contains("(-5)") && !msg.contains("Block not found") {
                        return Err(err).context("getblockheader failed after submit");
                    }
                }
            }
            if idx < 4 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }

        warn!("submitblock accepted but getblockheader timed out for {block_hash_hex}");
        Ok(SubmitBlockOutcome::Submitted)
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_millis() as u64
    }

    fn should_fire_block_zmq_trigger(&self) -> bool {
        const BLOCK_DEBOUNCE_MS: u64 = 10;
        let now = Self::now_ms();
        let last = self.last_zmq_block_trigger_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < BLOCK_DEBOUNCE_MS {
            return false;
        }
        self.last_zmq_block_trigger_ms.store(now, Ordering::Relaxed);
        true
    }

    fn spawn_refresh_task(&self, source: TemplateSource, trigger: &'static str) {
        let engine = self.clone();
        tokio::spawn(async move {
            if let Err(err) = engine.refresh_template(source).await {
                warn!("{trigger} template refresh failed source={source}: {err:?}");
            }
        });
    }

    async fn longpoll_loop(&self) {
        const LONGPOLL_TIMEOUT_SECS: u64 = 120;

        loop {
            let longpoll_id = { self.last_longpollid.lock().await.clone() };
            let params = match longpoll_id.as_deref() {
                Some(id) if !id.is_empty() => json!([{"rules": ["segwit"], "longpollid": id}]),
                _ => json!([{"rules": ["segwit"]}]),
            };

            match tokio::time::timeout(
                Duration::from_secs(LONGPOLL_TIMEOUT_SECS),
                self.rpc.call_longpoll::<GbtResponse>("getblocktemplate", params),
            )
            .await
            {
                Ok(Ok(gbt)) => {
                    if let Err(err) = self.apply_gbt_response(gbt, TemplateSource::Longpoll).await {
                        warn!("longpoll apply failed: {err:?}");
                    }
                }
                Ok(Err(err)) => {
                    warn!("longpoll GBT call failed: {err:?}; forcing one direct GBT refresh");
                    if let Err(refresh_err) =
                        self.refresh_template(TemplateSource::LongpollErrorFallback).await
                    {
                        warn!("direct refresh after longpoll failure failed: {refresh_err:?}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
                Err(_) => {
                    warn!(
                        "longpoll timed out after {LONGPOLL_TIMEOUT_SECS}s; forcing one direct GBT refresh"
                    );
                    if let Err(err) =
                        self.refresh_template(TemplateSource::LongpollTimeoutFallback).await
                    {
                        warn!("direct refresh after longpoll timeout failed: {err:?}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        }
    }

    async fn zmq_block_loop(&self, zmq_urls: Vec<String>) {
        let engine = self.clone();
        tokio::task::spawn_blocking(move || loop {
            let ctx = zmq::Context::new();
            let socket = match ctx.socket(zmq::SUB) {
                Ok(sock) => sock,
                Err(err) => {
                    warn!("ZMQ block socket create failed: {err:?}");
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };

            let mut connected = false;
            for url in &zmq_urls {
                match socket.connect(url) {
                    Ok(_) => {
                        info!("ZMQ block connected: {url}");
                        connected = true;
                    }
                    Err(err) => warn!("ZMQ block connect failed ({url}): {err:?}"),
                }
            }
            if !connected {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }

            let _ = socket.set_subscribe(b"hashblock");
            let _ = socket.set_subscribe(b"rawblock");

            loop {
                let topic = match socket.recv_msg(0) {
                    Ok(msg) => msg,
                    Err(err) => {
                        warn!("ZMQ block recv failed (topic): {err:?}; reconnecting");
                        break;
                    }
                };
                let source = topic.as_str().and_then(|value| match value {
                    "hashblock" => Some(TemplateSource::ZmqHashblock),
                    "rawblock" => Some(TemplateSource::ZmqRawblock),
                    _ => None,
                });

                if socket.recv_msg(0).is_err() {
                    if let Some(source) = source.filter(|_| engine.should_fire_block_zmq_trigger()) {
                        engine.counters.inc_zmq_block_received();
                        engine.counters.inc_zmq_blocks_detected();
                        info!(
                            "ZMQ block notification received source={source}; scheduling template refresh"
                        );
                        engine.spawn_refresh_task(source, "zmq block");
                    }
                    break;
                }

                let _ = socket.recv_msg(0);

                if let Some(source) = source {
                    engine.counters.inc_zmq_block_received();
                    if engine.should_fire_block_zmq_trigger() {
                        engine.counters.inc_zmq_blocks_detected();
                        info!(
                            "ZMQ block notification received source={source}; scheduling template refresh"
                        );
                        engine.spawn_refresh_task(source, "zmq block");
                    }
                }
            }

            warn!("ZMQ block socket lost; reconnecting in 1s");
            std::thread::sleep(Duration::from_secs(1));
        });
    }

    async fn refresh_template(&self, source: TemplateSource) -> anyhow::Result<()> {
        let mut source = source;
        loop {
            let gbt: GbtResponse = self
                .rpc
                .call("getblocktemplate", json!([{"rules": ["segwit"]}]))
                .await?;
            self.apply_gbt_response(gbt, source).await?;
            match self.take_pending_refresh_source() {
                Some(pending_source) => source = pending_source,
                None => break,
            }
        }
        Ok(())
    }

    async fn apply_gbt_response(
        &self,
        gbt: GbtResponse,
        source: TemplateSource,
    ) -> anyhow::Result<()> {
        let permit = match self.refresh_sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                self.store_pending_refresh_source(source);
                return Ok(());
            }
        };

        let txid_root = compute_txid_partial_root(&gbt.transactions);
        let key = format!(
            "{}:{}:{}:{}:{}:{}",
            gbt.previousblockhash,
            gbt.bits,
            gbt.version,
            gbt.longpollid.clone().unwrap_or_default(),
            gbt.submitold.unwrap_or(true),
            txid_root
        );

        if let Some(ref longpoll_id) = gbt.longpollid {
            let mut guard = self.last_longpollid.lock().await;
            *guard = Some(longpoll_id.clone());
        }

        let key_guard = self.last_template_key.lock().await;
        if key_guard.as_deref() == Some(&key) {
            self.counters.inc_notify_deduped();
            drop(key_guard);
            drop(permit);
            return Ok(());
        }
        drop(key_guard);

        let job = self
            .build_job(gbt, key.clone(), source, txid_root)
            .await?;
        self.update_chain_tip_from_job(&job).await;
        info!(
            "new template source={} height={} txs={} reward={} sat nbits={}",
            job.template_source,
            job.height,
            job.transactions.len(),
            job.coinbase_value,
            job.nbits
        );

        self.publish_job(job, key).await;

        drop(permit);
        Ok(())
    }

    fn store_pending_refresh_source(&self, source: TemplateSource) {
        self.refresh_pending_source
            .store(source.pending_slot(), Ordering::Release);
    }

    fn take_pending_refresh_source(&self) -> Option<TemplateSource> {
        TemplateSource::from_pending_slot(
            self.refresh_pending_source
                .swap(NO_PENDING_REFRESH_SOURCE, Ordering::AcqRel),
        )
    }

    async fn build_job(
        &self,
        gbt: GbtResponse,
        template_key: String,
        template_source: TemplateSource,
        txid_partial_root: String,
    ) -> anyhow::Result<JobTemplate> {
        let computed_witness_commitment =
            if let Some(ref core_commitment) = gbt.default_witness_commitment {
                let ours = compute_witness_commitment_script_hex(&gbt.transactions)?;
                if ours != *core_commitment {
                    warn!(
                        "witness commitment mismatch ours={} core={} using core value",
                        ours,
                        core_commitment
                    );
                    Some(core_commitment.clone())
                } else {
                    Some(ours)
                }
            } else {
                None
            };

        let (coinbase1, coinbase2, coinbase_tx, has_witness_commitment) = self.build_coinbase_parts(
            gbt.coinbasevalue,
            gbt.height,
            gbt.coinbaseaux.as_ref().and_then(|aux| aux.flags.as_deref()),
            computed_witness_commitment.as_deref(),
        )?;

        let coinbase_hash = double_sha256(&coinbase_tx);

        let mut tx_hashes = Vec::with_capacity(gbt.transactions.len());
        let mut tx_data = Vec::with_capacity(gbt.transactions.len());
        for tx in &gbt.transactions {
            let txid = tx
                .txid
                .as_ref()
                .ok_or_else(|| anyhow!("getblocktemplate transaction missing txid"))?;
            let mut hash = hex::decode(txid)?;
            hash.reverse();
            let hash_bytes: [u8; 32] =
                hash.try_into().map_err(|_| anyhow!("invalid txid length"))?;
            tx_hashes.push(hash_bytes);
            tx_data.push(tx.data.clone());
        }

        let merkle_branches_le = build_merkle_branches(coinbase_hash, &tx_hashes);
        let merkle_branches = merkle_branches_le.iter().map(hex::encode).collect::<Vec<_>>();

        let prevhash_bytes = hex::decode(&gbt.previousblockhash)?;
        let prevhash = swap_words(&prevhash_bytes)?;
        let prevhash_le = to_little_endian(&prevhash_bytes)?;
        let mut prevhash_le_vec = prevhash_bytes.clone();
        prevhash_le_vec.reverse();
        let prevhash_le_bytes: [u8; 32] = prevhash_le_vec
            .try_into()
            .map_err(|_| anyhow!("prevhash length must be 32"))?;

        let mut target_be = hex::decode(&gbt.target).context("decode target")?;
        if target_be.len() > 32 {
            target_be = target_be[target_be.len() - 32..].to_vec();
        }
        let mut target_be_padded = vec![0u8; 32 - target_be.len()];
        target_be_padded.extend_from_slice(&target_be);
        target_be_padded.reverse();
        let target_le: [u8; 32] = target_be_padded
            .try_into()
            .map_err(|_| anyhow!("target length must be 32"))?;

        let version = format!("{:08x}", gbt.version);
        let ntime = format!("{:08x}", gbt.curtime as u32);
        let mintime_u32 = gbt.mintime.unwrap_or(gbt.curtime) as u32;
        let coinbase1_bytes = hex::decode(&coinbase1).context("decode coinbase1")?;
        let coinbase2_bytes = hex::decode(&coinbase2).context("decode coinbase2")?;
        let nbits_u32 = u32::from_str_radix(&gbt.bits, 16).context("parse nbits")?;
        let network_difficulty = {
            let mantissa = (nbits_u32 & 0x007f_ffff) as f64;
            let exponent = ((nbits_u32 >> 24) & 0xff) as i32;
            let target_value = mantissa * 256.0f64.powi(exponent - 3);
            let diff1 = 65535.0 * 2.0f64.powi(208);
            if target_value > 0.0 {
                diff1 / target_value
            } else {
                0.0
            }
        };

        Ok(JobTemplate {
            ready: true,
            job_id: self.job_counter.fetch_add(1, Ordering::SeqCst).to_string(),
            submit_old: gbt.submitold.unwrap_or(true),
            prevhash,
            prevhash_le,
            coinbase1,
            coinbase2,
            merkle_branches,
            version,
            nbits: gbt.bits,
            ntime,
            height: gbt.height,
            transactions: tx_data,
            has_witness_commitment,
            coinbase_value: gbt.coinbasevalue,
            created_at: Utc::now(),
            prevhash_le_bytes,
            target_le,
            coinbase1_bytes,
            coinbase2_bytes,
            merkle_branches_le,
            version_u32: gbt.version,
            nbits_u32,
            mintime_u32,
            network_difficulty,
            template_key,
            template_source,
            txid_partial_root,
            witness_commitment_script: computed_witness_commitment,
        })
    }

    async fn update_chain_tip_from_job(&self, job: &JobTemplate) {
        if !job.ready || job.height == 0 || job.prevhash_le.is_empty() {
            return;
        }

        let mut hash_bytes = job.prevhash_le_bytes;
        hash_bytes.reverse();
        *self.chain_tip.write().await = Some(ChainTipState {
            hash_hex: hex::encode(hash_bytes),
            height: job.height.saturating_sub(1),
            median_time_past: job.mintime_u32.saturating_sub(1),
            nbits: job.nbits.clone(),
            nbits_u32: job.nbits_u32,
            target_le: job.target_le,
            version_u32: job.version_u32,
            network_difficulty: job.network_difficulty,
        });
    }

    async fn publish_job(&self, job: JobTemplate, key: String) {
        *self.last_template_key.lock().await = Some(key);
        let job = Arc::new(job);
        {
            let mut guard = self.current.write().await;
            *guard = job.clone();
        }
        let _ = self.sender.send(job);
    }

    fn build_empty_job(
        &self,
        tip: &ChainTipState,
        block_hash_hex: &str,
        next_height: u64,
        estimated_mtp: u32,
        template_key: String,
    ) -> anyhow::Result<JobTemplate> {
        let subsidy = block_subsidy_sat(next_height);
        let (coinbase1, coinbase2, _coinbase_tx, has_witness_commitment) =
            self.build_coinbase_parts(subsidy, next_height, None, None)?;
        let block_hash_bytes =
            hex::decode(block_hash_hex).context("decode fast block announcement hash")?;
        let prevhash = swap_words(&block_hash_bytes)?;
        let prevhash_le = to_little_endian(&block_hash_bytes)?;
        let mut prevhash_le_vec = block_hash_bytes;
        prevhash_le_vec.reverse();
        let prevhash_le_bytes: [u8; 32] = prevhash_le_vec
            .try_into()
            .map_err(|_| anyhow!("prevhash length must be 32"))?;
        let coinbase1_bytes = hex::decode(&coinbase1).context("decode coinbase1")?;
        let coinbase2_bytes = hex::decode(&coinbase2).context("decode coinbase2")?;
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs() as u32;
        let mintime_u32 = estimated_mtp.saturating_add(1);
        let ntime_u32 = now_secs.max(mintime_u32);

        Ok(JobTemplate {
            ready: true,
            job_id: self.job_counter.fetch_add(1, Ordering::SeqCst).to_string(),
            submit_old: false,
            prevhash,
            prevhash_le,
            coinbase1,
            coinbase2,
            merkle_branches: vec![],
            version: format!("{:08x}", tip.version_u32),
            nbits: tip.nbits.clone(),
            ntime: format!("{:08x}", ntime_u32),
            height: next_height,
            transactions: vec![],
            has_witness_commitment,
            coinbase_value: subsidy,
            created_at: Utc::now(),
            prevhash_le_bytes,
            target_le: tip.target_le,
            coinbase1_bytes,
            coinbase2_bytes,
            merkle_branches_le: vec![],
            version_u32: tip.version_u32,
            nbits_u32: tip.nbits_u32,
            mintime_u32,
            network_difficulty: tip.network_difficulty,
            template_key,
            template_source: TemplateSource::P2pCmpctFast,
            txid_partial_root: "0".repeat(64),
            witness_commitment_script: None,
        })
    }

    fn build_coinbase_parts(
        &self,
        coinbase_value: u64,
        height: u64,
        coinbase_flags: Option<&str>,
        witness_commitment_hex: Option<&str>,
    ) -> anyhow::Result<(String, String, Vec<u8>, bool)> {
        let script_pubkey = if let Some(script_hex) = &self.config.payout_script_hex {
            hex::decode(script_hex).context("decode payout script")?
        } else {
            let address = Address::from_str(&self.config.payout_address)
                .context("parse payout address")?
                .require_network(self.config.network)
                .context("payout address network mismatch")?;
            address.script_pubkey().as_bytes().to_vec()
        };

        let mut script_sig = Vec::new();
        script_sig.extend_from_slice(&encode_coinbase_height(height)?);

        if let Some(flags_hex) = coinbase_flags {
            let flags = hex::decode(flags_hex).context("decode coinbaseaux flags")?;
            if !flags.is_empty() {
                encode_script_pushdata(&flags, &mut script_sig)?;
            }
        }

        let extranonce1 = vec![0u8; self.config.extranonce1_size];
        let extranonce2 = vec![0u8; self.config.extranonce2_size];
        let mut tag = self.config.pool_tag.clone().into_bytes();
        if !self.config.coinbase_message.is_empty() {
            tag.extend_from_slice(b"/");
            tag.extend_from_slice(self.config.coinbase_message.as_bytes());
        }
        if !tag.is_empty() {
            encode_script_pushdata(&tag, &mut script_sig)?;
        }

        let total_script_sig =
            script_sig.len() + self.config.extranonce1_size + self.config.extranonce2_size;
        if total_script_sig > 100 {
            return Err(anyhow!(
                "coinbase scriptSig too long: {} bytes; shorten pool_tag or coinbase_message",
                total_script_sig
            ));
        }

        let witness_commitment = if let Some(value) = witness_commitment_hex {
            Some(hex::decode(value).context("decode witness commitment")?)
        } else {
            None
        };
        let has_witness_commitment = witness_commitment.is_some();

        let mut coinbase_tx = Vec::new();
        coinbase_tx.extend_from_slice(&2u32.to_le_bytes());
        encode_varint(1, &mut coinbase_tx);
        coinbase_tx.extend_from_slice(&[0u8; 32]);
        coinbase_tx.extend_from_slice(&0xffff_ffffu32.to_le_bytes());

        let mut script_sig_full = script_sig.clone();
        script_sig_full.extend_from_slice(&extranonce1);
        script_sig_full.extend_from_slice(&extranonce2);

        encode_varint(script_sig_full.len() as u64, &mut coinbase_tx);
        coinbase_tx.extend_from_slice(&script_sig_full);
        coinbase_tx.extend_from_slice(&0xffff_ffffu32.to_le_bytes());

        let output_count = if has_witness_commitment { 2 } else { 1 };
        encode_varint(output_count, &mut coinbase_tx);
        coinbase_tx.extend_from_slice(&coinbase_value.to_le_bytes());
        encode_varint(script_pubkey.len() as u64, &mut coinbase_tx);
        coinbase_tx.extend_from_slice(&script_pubkey);

        if let Some(commitment) = witness_commitment {
            coinbase_tx.extend_from_slice(&0u64.to_le_bytes());
            encode_varint(commitment.len() as u64, &mut coinbase_tx);
            coinbase_tx.extend_from_slice(&commitment);
        }

        let locktime = height.saturating_sub(1).min(u32::MAX as u64) as u32;
        coinbase_tx.extend_from_slice(&locktime.to_le_bytes());

        let extra_start = script_sig.len();
        let extra_end = extra_start + self.config.extranonce1_size + self.config.extranonce2_size;
        let mut cursor = 0usize;
        cursor += 4;
        cursor += varint_len(1);
        cursor += 32 + 4;
        cursor += varint_len(script_sig_full.len() as u64);
        let script_start = cursor;
        let prefix_end = script_start + extra_start;
        let suffix_start = script_start + extra_end;

        Ok((
            hex::encode(&coinbase_tx[..prefix_end]),
            hex::encode(&coinbase_tx[suffix_start..]),
            coinbase_tx,
            has_witness_commitment,
        ))
    }
}

fn compute_witness_commitment_script_hex(txs: &[GbtTx]) -> anyhow::Result<String> {
    let mut leaves: Vec<[u8; 32]> = Vec::with_capacity(txs.len() + 1);
    leaves.push([0u8; 32]);

    for tx in txs {
        let wtxid_hex = tx
            .hash
            .as_ref()
            .ok_or_else(|| anyhow!("getblocktemplate transaction missing wtxid hash"))?;
        let mut bytes = hex::decode(wtxid_hex).context("decode wtxid")?;
        if bytes.len() != 32 {
            return Err(anyhow!("invalid wtxid length"));
        }
        bytes.reverse();
        leaves.push(bytes.try_into().unwrap());
    }

    let mut level = leaves;
    while level.len() > 1 {
        let mut next = Vec::with_capacity((level.len() + 1) / 2);
        for idx in (0..level.len()).step_by(2) {
            let left = level[idx];
            let right = if idx + 1 < level.len() { level[idx + 1] } else { left };
            next.push(merkle_step(&left, &right));
        }
        level = next;
    }

    let witness_merkle_root = level[0];
    let reserved = [0u8; 32];
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&witness_merkle_root);
    buf[32..].copy_from_slice(&reserved);
    let commitment = double_sha256(&buf);

    let mut script = Vec::with_capacity(38);
    script.push(0x6a);
    script.push(0x24);
    script.extend_from_slice(&[0xaa, 0x21, 0xa9, 0xed]);
    script.extend_from_slice(&commitment);
    Ok(hex::encode(script))
}

fn encode_coinbase_height(height: u64) -> anyhow::Result<Vec<u8>> {
    if height > i64::MAX as u64 {
        return Err(anyhow!("coinbase height exceeds i64 range"));
    }

    if height == 0 {
        return Ok(vec![0x00]);
    }

    let height_bytes = encode_script_num(height as i64);
    let mut out = Vec::with_capacity(height_bytes.len() + 1);
    out.push(height_bytes.len() as u8);
    out.extend_from_slice(&height_bytes);
    Ok(out)
}

fn compute_txid_partial_root(transactions: &[GbtTx]) -> String {
    if transactions.is_empty() {
        return "0".repeat(64);
    }
    let mut hashes: Vec<[u8; 32]> = transactions
        .iter()
        .filter_map(|tx| {
            let txid = tx.txid.as_ref()?;
            let mut bytes = hex::decode(txid).ok()?;
            bytes.reverse();
            bytes.try_into().ok()
        })
        .collect();
    if hashes.is_empty() {
        return "0".repeat(64);
    }
    while hashes.len() > 1 {
        if hashes.len() % 2 == 1 {
            hashes.push(*hashes.last().unwrap());
        }
        hashes = hashes
            .chunks(2)
            .map(|pair| merkle_step(&pair[0], &pair[1]))
            .collect();
    }
    let mut root = hashes[0];
    root.reverse();
    hex::encode(root)
}

fn categorise_reject_reason(reason: &str) -> &'static str {
    if reason.contains("mrklroot") || reason.contains("merkle") {
        "merkle"
    } else if reason.contains("witness") {
        "witness"
    } else if reason.contains("cb-") {
        "coinbase"
    } else if reason.contains("time") {
        "time"
    } else if reason.contains("prevblk") {
        "stale"
    } else if reason.contains("diffbits") {
        "difficulty"
    } else {
        "unknown"
    }
}

fn block_subsidy_sat(height: u64) -> u64 {
    let halvings = height / 210_000;
    if halvings >= 64 {
        0
    } else {
        (50 * 100_000_000u64) >> halvings
    }
}

fn build_merkle_branches(coinbase_hash: [u8; 32], tx_hashes: &[[u8; 32]]) -> Vec<[u8; 32]> {
    let mut level: Vec<[u8; 32]> = std::iter::once(coinbase_hash)
        .chain(tx_hashes.iter().copied())
        .collect();
    let mut branches = Vec::new();

    while level.len() > 1 {
        branches.push(level[1]);
        let mut next = Vec::with_capacity((level.len() + 1) / 2);
        for idx in (0..level.len()).step_by(2) {
            let left = level[idx];
            let right = if idx + 1 < level.len() { level[idx + 1] } else { left };
            next.push(merkle_step(&left, &right));
        }
        level = next;
    }

    branches
}

fn swap_words(bytes: &[u8]) -> anyhow::Result<String> {
    if bytes.len() != 32 {
        return Err(anyhow!("prevhash must be 32 bytes"));
    }
    let mut out = [0u8; 32];
    for idx in 0..8 {
        let start = idx * 4;
        let src = 28 - (idx * 4);
        out[start..start + 4].copy_from_slice(&bytes[src..src + 4]);
    }
    Ok(hex::encode(out))
}

fn to_little_endian(bytes: &[u8]) -> anyhow::Result<String> {
    if bytes.len() != 32 {
        return Err(anyhow!("prevhash must be 32 bytes"));
    }
    let mut out = bytes.to_vec();
    out.reverse();
    Ok(hex::encode(out))
}

fn encode_script_num(value: i64) -> Vec<u8> {
    if value == 0 {
        return vec![];
    }

    let mut out = Vec::new();
    let mut abs_value = value.unsigned_abs();
    while abs_value > 0 {
        out.push((abs_value & 0xff) as u8);
        abs_value >>= 8;
    }

    let is_negative = value < 0;
    if let Some(last) = out.last_mut() {
        if *last & 0x80 != 0 {
            out.push(if is_negative { 0x80 } else { 0x00 });
        } else if is_negative {
            *last |= 0x80;
        }
    }

    out
}

fn encode_script_pushdata(data: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
    match data.len() {
        0..=75 => out.push(data.len() as u8),
        76..=0xff => {
            out.push(0x4c);
            out.push(data.len() as u8);
        }
        0x100..=0xffff => {
            out.push(0x4d);
            out.extend_from_slice(&(data.len() as u16).to_le_bytes());
        }
        _ => {
            return Err(anyhow!(
                "script pushdata too long: {} bytes",
                data.len()
            ));
        }
    }
    out.extend_from_slice(data);
    Ok(())
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

#[inline]
fn varint_len(value: u64) -> usize {
    match value {
        0..=0xfc => 1,
        0xfd..=0xffff => 3,
        0x1_0000..=0xffff_ffff => 5,
        _ => 9,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coinbase_height_uses_pushdata_encoding_for_small_heights() {
        assert_eq!(encode_coinbase_height(1).unwrap(), vec![0x01, 0x01]);
        assert_eq!(encode_coinbase_height(16).unwrap(), vec![0x01, 0x10]);
    }

    #[test]
    fn script_pushdata_uses_single_byte_length_for_small_payloads() {
        let mut encoded = Vec::new();
        let payload = vec![0x11; 75];

        encode_script_pushdata(&payload, &mut encoded).unwrap();

        assert_eq!(encoded[0], 75);
        assert_eq!(&encoded[1..], payload.as_slice());
    }

    #[test]
    fn script_pushdata_uses_op_pushdata1_for_76_byte_payloads() {
        let mut encoded = Vec::new();
        let payload = vec![0x22; 76];

        encode_script_pushdata(&payload, &mut encoded).unwrap();

        assert_eq!(encoded[0], 0x4c);
        assert_eq!(encoded[1], 76);
        assert_eq!(&encoded[2..], payload.as_slice());
    }
}
