use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::extract::State;
use axum::http::{header::CONTENT_TYPE, Method, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use bitcoin::{base58, Network};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};

use crate::config::Config;
use crate::metrics::{BlockEvent, MetricsStore, MinerStats, ShareEvent};
use crate::rpc::RpcClient;
use crate::stratum::{MinerLatencyRegistry, MinerLatencySample};
use crate::template::{JobTemplate, TemplateEngine, TemplateSource};

const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");
const GATEWAY_VERSION: &str = concat!("StratumBee ", env!("CARGO_PKG_VERSION"));
const HEALTH_RPC_TIMEOUT_SECS: u64 = 3;
const HEALTH_MAX_GBT_AGE_SECS: u64 = 180;

#[derive(Clone)]
pub struct ApiServer {
    config: Config,
    metrics: MetricsStore,
    rpc: RpcClient,
    template_engine: Arc<TemplateEngine>,
    latency_registry: MinerLatencyRegistry,
}

impl ApiServer {
    pub fn new(
        config: Config,
        metrics: MetricsStore,
        rpc: RpcClient,
        template_engine: Arc<TemplateEngine>,
        latency_registry: MinerLatencyRegistry,
    ) -> Self {
        Self {
            config,
            metrics,
            rpc,
            template_engine,
            latency_registry,
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let state = ApiState {
            metrics: self.metrics.clone(),
            rpc: self.rpc.clone(),
            template_engine: self.template_engine.clone(),
            network_name: network_label(self.config.network).to_string(),
            latency_registry: self.latency_registry.clone(),
            sv2_authority_public_key: load_sv2_authority_public_key(&self.config),
        };

        let app = Router::new()
            .route("/", get(index))
            .route("/health", get(health))
            .route("/api/stats", get(stats))
            .route("/api/workers", get(workers))
            .route("/api/v1/summary", get(summary))
            .route("/api/v1/miners", get(miners))
            .route("/api/v1/blocks", get(blocks))
            .route("/api/v1/events", get(events))
            .route("/api/v1/network", get(network))
            .route("/api/v1/template", get(template))
            .route("/api/v1/mempool", get(mempool))
            .route("/api/v1/latency", get(latency))
            .route("/metrics", get(prometheus_metrics))
            .with_state(state)
            .layer(
                CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods([Method::GET, Method::HEAD])
                    .allow_headers([CONTENT_TYPE]),
            );

        let bind = format!("{}:{}", self.config.api_bind, self.config.api_port);
        info!("api listening on {bind}");
        let listener = tokio::net::TcpListener::bind(&bind).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct ApiState {
    metrics: MetricsStore,
    rpc: RpcClient,
    template_engine: Arc<TemplateEngine>,
    network_name: String,
    latency_registry: MinerLatencyRegistry,
    sv2_authority_public_key: Option<Sv2AuthorityPublicKey>,
}

#[derive(Clone)]
struct Sv2AuthorityPublicKey {
    encoded: String,
}

async fn index() -> impl IntoResponse {
    Html(DASHBOARD_HTML)
}

async fn health(State(state): State<ApiState>) -> impl IntoResponse {
    let job = state.template_engine.current_job().await;
    let last_gbt_success_age_secs = state.template_engine.last_gbt_success_age_secs();
    let template_fresh =
        last_gbt_success_age_secs.is_some_and(|age| age <= HEALTH_MAX_GBT_AGE_SECS);
    let rpc_result = tokio::time::timeout(
        StdDuration::from_secs(HEALTH_RPC_TIMEOUT_SECS),
        state
            .rpc
            .call::<serde_json::Value>("getblockchaininfo", serde_json::json!([])),
    )
    .await;
    let rpc_info = match rpc_result {
        Ok(Ok(value)) => Some(value),
        _ => None,
    };
    let rpc_blocks = rpc_info
        .as_ref()
        .and_then(|value| value.get("blocks"))
        .and_then(|value| value.as_u64());
    let template_matches_tip =
        rpc_blocks.is_some_and(|blocks| template_matches_core_tip(job.as_ref(), blocks));
    let rpc_ok = rpc_info.is_some();
    let ok = job.ready && template_fresh && rpc_ok && template_matches_tip;
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(serde_json::json!({
            "ok": ok,
            "template_ready": job.ready,
            "template_fresh": template_fresh,
            "template_matches_tip": template_matches_tip,
            "template_height": job.height,
            "last_gbt_success_age_secs": last_gbt_success_age_secs,
            "rpc_ok": rpc_ok,
            "rpc_blocks": rpc_blocks,
            "uptime_secs": (Utc::now() - state.metrics.started_at).num_seconds().max(0),
        })),
    )
}

#[derive(Serialize)]
struct SummaryResponse {
    uptime_secs: u64,
    total_hashrate_gh: f64,
    total_blocks: u64,
    accepted_blocks: u64,
    confirmed_blocks: u64,
    matured_blocks: u64,
    connected_miners: usize,
    current_height: u64,
    current_reward_sat: u64,
    current_tx_count: usize,
    network_difficulty: f64,
    current_prevhash: String,
    global_best_diff: f64,
    global_best_diff_worker: Option<String>,
    zmq_blocks_detected: u64,
    zmq_block_notifications: u64,
    jobs_sent: u64,
    clean_jobs_sent: u64,
    notify_deduped: u64,
    notify_rate_limited: u64,
    duplicate_shares: u64,
    reconnects_total: u64,
    submitblock_accepted: u64,
    submitblock_rejected: u64,
    submitblock_rpc_fail: u64,
    version_rolling_violations: u64,
    stales_new_block: u64,
    stales_expired: u64,
    stales_reconnect: u64,
    sv2_connections_total: u64,
    sv2_connections_active: u64,
    sv2_connections_closed: u64,
    sv2_rejected_max_connections: u64,
    sv2_rejected_per_ip: u64,
    sv2_handshake_success: u64,
    sv2_handshake_failures: u64,
    sv2_setup_accepted: u64,
    sv2_setup_rejected: u64,
    sv2_channels_opened: u64,
    sv2_channel_open_errors: u64,
    sv2_channels_closed: u64,
    sv2_unsupported_messages: u64,
    sv2_submit_accepted: u64,
    sv2_submit_rejected: u64,
    sv2_blocks_found: u64,
    sv2_authority_public_key: Option<String>,
}

async fn summary(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    let job = state.template_engine.current_job().await;
    let c = &state.metrics.counters;
    let global_best_diff_worker = snapshot.global_best_worker.as_ref().map(|worker| {
        snapshot
            .miners
            .iter()
            .find(|miner| &miner.worker == worker)
            .map(public_miner_label)
            .unwrap_or_else(|| "Hidden Miner".to_string())
    });
    Json(SummaryResponse {
        uptime_secs: (Utc::now() - state.metrics.started_at).num_seconds().max(0) as u64,
        total_hashrate_gh: snapshot.total_hashrate_gh,
        total_blocks: snapshot.total_blocks,
        accepted_blocks: snapshot.accepted_blocks,
        confirmed_blocks: snapshot.confirmed_blocks,
        matured_blocks: snapshot.matured_blocks,
        connected_miners: snapshot.miners.len(),
        current_height: job.height,
        current_reward_sat: job.coinbase_value,
        current_tx_count: job.transactions.len(),
        network_difficulty: job.network_difficulty,
        current_prevhash: display_prevhash(job.as_ref()),
        global_best_diff: snapshot.global_best_difficulty,
        global_best_diff_worker,
        zmq_blocks_detected: c.zmq_blocks_detected(),
        zmq_block_notifications: c.zmq_block_received(),
        jobs_sent: c.jobs_sent(),
        clean_jobs_sent: c.clean_jobs_sent(),
        notify_deduped: c.notify_deduped(),
        notify_rate_limited: c.notify_rate_limited(),
        duplicate_shares: c.duplicate_shares(),
        reconnects_total: c.reconnects_total(),
        submitblock_accepted: c.submitblock_accepted(),
        submitblock_rejected: c.submitblock_rejected(),
        submitblock_rpc_fail: c.submitblock_rpc_fail(),
        version_rolling_violations: c.version_rolling_violations(),
        stales_new_block: c.stales_new_block(),
        stales_expired: c.stales_expired(),
        stales_reconnect: c.stales_reconnect(),
        sv2_connections_total: c.sv2_connections_total(),
        sv2_connections_active: c.sv2_connections_active(),
        sv2_connections_closed: c.sv2_connections_closed(),
        sv2_rejected_max_connections: c.sv2_rejected_max_connections(),
        sv2_rejected_per_ip: c.sv2_rejected_per_ip(),
        sv2_handshake_success: c.sv2_handshake_success(),
        sv2_handshake_failures: c.sv2_handshake_failures(),
        sv2_setup_accepted: c.sv2_setup_accepted(),
        sv2_setup_rejected: c.sv2_setup_rejected(),
        sv2_channels_opened: c.sv2_channels_opened(),
        sv2_channel_open_errors: c.sv2_channel_open_errors(),
        sv2_channels_closed: c.sv2_channels_closed(),
        sv2_unsupported_messages: c.sv2_unsupported_messages(),
        sv2_submit_accepted: c.sv2_submit_accepted(),
        sv2_submit_rejected: c.sv2_submit_rejected(),
        sv2_blocks_found: c.sv2_blocks_found(),
        sv2_authority_public_key: state
            .sv2_authority_public_key
            .as_ref()
            .map(|key| key.encoded.clone()),
    })
}

#[derive(Serialize)]
struct StatsResponse {
    gateway_version: &'static str,
    uptime_seconds: u64,
    hashrate_hs: f64,
    hashrate_th_s: f64,
    connections: usize,
    subscriptions: usize,
    current_height: u64,
    current_value_btc: f64,
    txn_count: usize,
    network_difficulty: f64,
    total_blocks: u64,
    accepted_blocks: u64,
    confirmed_blocks: u64,
    matured_blocks: u64,
    template_ready: bool,
}

async fn stats(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    let job = state.template_engine.current_job().await;
    let connected = snapshot.miners.len();
    let hashrate_hs = snapshot.total_hashrate_gh * 1_000_000_000.0;

    Json(StatsResponse {
        gateway_version: GATEWAY_VERSION,
        uptime_seconds: (Utc::now() - state.metrics.started_at).num_seconds().max(0) as u64,
        hashrate_hs,
        hashrate_th_s: hashrate_hs / 1_000_000_000_000.0,
        connections: connected,
        subscriptions: connected,
        current_height: job.height,
        current_value_btc: job.coinbase_value as f64 / 100_000_000.0,
        txn_count: job.transactions.len(),
        network_difficulty: job.network_difficulty,
        total_blocks: snapshot.total_blocks,
        accepted_blocks: snapshot.accepted_blocks,
        confirmed_blocks: snapshot.confirmed_blocks,
        matured_blocks: snapshot.matured_blocks,
        template_ready: job.ready,
    })
}

#[derive(Serialize)]
struct WorkerResponse {
    id: String,
    user_agent: String,
    protocol: String,
    display_name: String,
    hashrate: f64,
    hashrate_hs: f64,
    best_diff: f64,
    current_diff: f64,
    shares: u64,
    last_seen: chrono::DateTime<Utc>,
}

async fn workers(State(state): State<ApiState>) -> impl IntoResponse {
    let workers = state
        .metrics
        .snapshot()
        .await
        .miners
        .into_iter()
        .map(|miner| {
            let id = public_miner_id(&miner);
            let user_agent = public_user_agent(miner.user_agent.as_deref());
            let protocol = public_protocol(miner.protocol.as_deref());
            let display_name = public_miner_label(&miner);

            WorkerResponse {
                id,
                user_agent,
                protocol,
                display_name,
                hashrate: miner.hashrate_gh / 1_000.0,
                hashrate_hs: miner.hashrate_gh * 1_000_000_000.0,
                best_diff: miner.best_difficulty,
                current_diff: miner.difficulty,
                shares: miner.shares,
                last_seen: miner.last_seen,
            }
        })
        .collect::<Vec<_>>();
    Json(workers)
}

#[derive(Serialize)]
struct MinerResponse {
    id: String,
    user_agent: String,
    protocol: String,
    display_name: String,
    difficulty: f64,
    best_difficulty: f64,
    best_submitted_difficulty: f64,
    shares: u64,
    rejected: u64,
    stale: u64,
    hashrate_gh: f64,
    last_seen: DateTime<Utc>,
    notify_to_submit_ms: f64,
    submit_rtt_ms: f64,
    last_share_time: Option<DateTime<Utc>>,
}

async fn miners(State(state): State<ApiState>) -> impl IntoResponse {
    let miners = state
        .metrics
        .snapshot()
        .await
        .miners
        .into_iter()
        .map(miner_response)
        .collect::<Vec<_>>();
    Json(miners)
}

async fn blocks(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    let blocks = state
        .metrics
        .recent_blocks()
        .await
        .into_iter()
        .map(|block| block_response(block, &snapshot.miners))
        .collect::<Vec<_>>();
    Json(blocks)
}

#[derive(Serialize)]
struct ShareEventResponse {
    miner_id: String,
    user_agent: String,
    protocol: String,
    difficulty: f64,
    accepted: bool,
    is_block: bool,
    created_at: DateTime<Utc>,
    job_age_secs: u64,
    notify_delay_ms: u64,
    reconnect_recent: bool,
}

async fn events(State(state): State<ApiState>) -> impl IntoResponse {
    let events: Vec<ShareEvent> = state.metrics.recent_events(Duration::minutes(30)).await;
    Json(
        events
            .into_iter()
            .map(share_event_response)
            .collect::<Vec<_>>(),
    )
}

#[derive(Serialize)]
struct BlockResponse {
    height: u64,
    hash: String,
    worker: String,
    difficulty: f64,
    status: String,
    reason: Option<String>,
    confirmations: Option<i64>,
    in_active_chain: Option<bool>,
    reward_status: String,
    created_at: DateTime<Utc>,
}

fn miner_response(miner: MinerStats) -> MinerResponse {
    MinerResponse {
        id: public_miner_id(&miner),
        user_agent: public_user_agent(miner.user_agent.as_deref()),
        protocol: public_protocol(miner.protocol.as_deref()),
        display_name: public_miner_label(&miner),
        difficulty: miner.difficulty,
        best_difficulty: miner.best_difficulty,
        best_submitted_difficulty: miner.best_submitted_difficulty,
        shares: miner.shares,
        rejected: miner.rejected,
        stale: miner.stale,
        hashrate_gh: miner.hashrate_gh,
        last_seen: miner.last_seen,
        notify_to_submit_ms: miner.notify_to_submit_ms,
        submit_rtt_ms: miner.submit_rtt_ms,
        last_share_time: miner.last_share_time,
    }
}

fn share_event_response(event: ShareEvent) -> ShareEventResponse {
    ShareEventResponse {
        miner_id: public_event_miner_id(&event),
        user_agent: public_user_agent(event.user_agent.as_deref()),
        protocol: public_protocol(event.protocol.as_deref()),
        difficulty: event.difficulty,
        accepted: event.accepted,
        is_block: event.is_block,
        created_at: event.created_at,
        job_age_secs: event.job_age_secs,
        notify_delay_ms: event.notify_delay_ms,
        reconnect_recent: event.reconnect_recent,
    }
}

fn block_response(block: BlockEvent, miners: &[MinerStats]) -> BlockResponse {
    let worker = miners
        .iter()
        .find(|miner| miner.worker == block.worker)
        .map(public_miner_label)
        .unwrap_or_else(|| "Hidden Miner".to_string());

    BlockResponse {
        height: block.height,
        hash: block.hash,
        worker,
        difficulty: block.difficulty,
        status: block.status,
        reason: block.reason,
        confirmations: block.confirmations,
        in_active_chain: block.in_active_chain,
        reward_status: block.reward_status,
        created_at: block.created_at,
    }
}

fn public_miner_label(miner: &MinerStats) -> String {
    if miner.protocol.as_deref() == Some("SV2") {
        return format!(
            "{} / {}",
            public_protocol(miner.protocol.as_deref()),
            public_optional_text(Some(&miner.worker))
                .unwrap_or_else(|| public_user_agent(miner.user_agent.as_deref()))
        );
    }

    format!(
        "{} / {}",
        public_protocol(miner.protocol.as_deref()),
        public_user_agent(miner.user_agent.as_deref())
    )
}

fn public_miner_id(miner: &MinerStats) -> String {
    if miner.protocol.as_deref() == Some("SV1") {
        return public_optional_text(Some(&miner.worker)).unwrap_or_else(|| "miner".to_string());
    }
    public_optional_text(miner.session_id.as_deref()).unwrap_or_else(|| "miner".to_string())
}

fn public_event_miner_id(event: &ShareEvent) -> String {
    public_optional_text(event.session_id.as_deref()).unwrap_or_else(|| "miner".to_string())
}

fn public_protocol(protocol: Option<&str>) -> String {
    public_optional_text(protocol).unwrap_or_else(|| "Unknown".to_string())
}

fn public_user_agent(user_agent: Option<&str>) -> String {
    public_optional_text(user_agent).unwrap_or_else(|| "Unknown Miner".to_string())
}

fn public_optional_text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[derive(Serialize)]
struct NetworkResponse {
    network: String,
    blocks: Option<u64>,
    difficulty: Option<f64>,
    networkhashps: Option<f64>,
}

async fn network(State(state): State<ApiState>) -> impl IntoResponse {
    let info = state
        .rpc
        .call::<serde_json::Value>("getmininginfo", serde_json::json!([]))
        .await
        .ok();
    let response = NetworkResponse {
        network: state.network_name.clone(),
        blocks: info
            .as_ref()
            .and_then(|v| v.get("blocks"))
            .and_then(|v| v.as_u64()),
        difficulty: info
            .as_ref()
            .and_then(|v| v.get("difficulty"))
            .and_then(|v| v.as_f64()),
        networkhashps: info
            .as_ref()
            .and_then(|v| v.get("networkhashps"))
            .and_then(|v| v.as_f64()),
    };
    Json(response)
}

fn network_label(network: Network) -> &'static str {
    match network {
        Network::Bitcoin => "Mainnet",
        Network::Testnet => "Testnet",
        Network::Signet => "Signet",
        _ => "Unknown",
    }
}

fn display_prevhash(job: &JobTemplate) -> String {
    if !job.ready || job.prevhash_le.is_empty() {
        return String::new();
    }
    let mut bytes = job.prevhash_le_bytes.to_vec();
    bytes.reverse();
    hex::encode(bytes)
}

fn template_matches_core_tip(job: &JobTemplate, core_blocks: u64) -> bool {
    let expected_height = core_blocks.saturating_add(1);
    job.height == expected_height
        || (job.template_source == TemplateSource::P2pCmpctFast
            && job.height == expected_height.saturating_add(1))
}

fn load_sv2_authority_public_key(config: &Config) -> Option<Sv2AuthorityPublicKey> {
    let path = config.sv2_authority_public_key_path.as_deref()?;
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            warn!(path = %path.display(), "failed to read sv2 authority public key for dashboard: {err}");
            return None;
        }
    };
    let normalized = raw
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .trim_start_matches("0x")
        .to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.bytes().all(|b| b.is_ascii_hexdigit()) {
        warn!(path = %path.display(), "sv2 authority public key is not 32-byte hex");
        return None;
    }
    let bytes = match hex::decode(&normalized) {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!(path = %path.display(), "failed to decode sv2 authority public key: {err}");
            return None;
        }
    };
    let public_key: [u8; 32] = match bytes.try_into() {
        Ok(public_key) => public_key,
        Err(_) => {
            warn!(path = %path.display(), "sv2 authority public key is not 32-byte hex");
            return None;
        }
    };
    Some(Sv2AuthorityPublicKey {
        encoded: encode_sv2_authority_public_key(&public_key),
    })
}

fn encode_sv2_authority_public_key(public_key: &[u8; 32]) -> String {
    let mut payload = Vec::with_capacity(34);
    payload.extend_from_slice(&[1, 0]);
    payload.extend_from_slice(public_key);
    base58::encode_check(&payload)
}

#[derive(Serialize)]
struct TemplateResponse {
    ready: bool,
    job_id: String,
    height: u64,
    version: String,
    bits: String,
    ntime: String,
    prevhash: String,
    network_difficulty: f64,
    tx_count: usize,
    coinbase_value: u64,
    witness_commitment: Option<String>,
}

async fn template(State(state): State<ApiState>) -> impl IntoResponse {
    let job = state.template_engine.current_job().await;
    Json(TemplateResponse {
        ready: job.ready,
        job_id: job.job_id.clone(),
        height: job.height,
        version: job.version.clone(),
        bits: job.nbits.clone(),
        ntime: job.ntime.clone(),
        prevhash: job.prevhash.clone(),
        network_difficulty: job.network_difficulty,
        tx_count: job.transactions.len(),
        coinbase_value: job.coinbase_value,
        witness_commitment: job.witness_commitment_script.clone(),
    })
}

async fn mempool(State(state): State<ApiState>) -> impl IntoResponse {
    let value = state
        .rpc
        .call::<serde_json::Value>("getmempoolinfo", serde_json::json!([]))
        .await
        .unwrap_or_else(|_| serde_json::json!({}));
    Json(value)
}

async fn latency(State(state): State<ApiState>) -> impl IntoResponse {
    let mut samples = state.latency_registry.sample_all().await;
    let mut sampled_workers = samples
        .iter()
        .map(|sample| sample.worker.clone())
        .collect::<HashSet<_>>();

    let now = Utc::now();
    for miner in state.metrics.snapshot().await.miners {
        if miner.protocol.as_deref() != Some("SV2") {
            continue;
        }

        let worker = public_miner_id(&miner);
        if sampled_workers.insert(worker.clone()) {
            samples.push(MinerLatencySample {
                worker,
                latency_ms: None,
                sampled_at: now,
                peer_addr: None,
                error: Some("tcp latency unsupported for SV2".to_string()),
            });
        }
    }

    samples.sort_by(|a, b| a.worker.cmp(&b.worker));
    Json(samples)
}

async fn prometheus_metrics(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    let job = state.template_engine.current_job().await;
    let c = &state.metrics.counters;
    let mut out = String::new();

    push_metric(
        &mut out,
        "stratumbee_uptime_seconds",
        (Utc::now() - state.metrics.started_at).num_seconds().max(0),
    );
    push_metric(
        &mut out,
        "stratumbee_template_ready",
        if job.ready { 1 } else { 0 },
    );
    push_metric(&mut out, "stratumbee_current_height", job.height);
    push_metric(
        &mut out,
        "stratumbee_current_tx_count",
        job.transactions.len(),
    );
    push_metric(
        &mut out,
        "stratumbee_current_reward_sat",
        job.coinbase_value,
    );
    push_metric(
        &mut out,
        "stratumbee_connected_miners",
        snapshot.miners.len(),
    );
    push_metric(
        &mut out,
        "stratumbee_total_hashrate_gh",
        snapshot.total_hashrate_gh,
    );
    push_metric(&mut out, "stratumbee_total_blocks", snapshot.total_blocks);
    push_metric(
        &mut out,
        "stratumbee_accepted_blocks",
        snapshot.accepted_blocks,
    );
    push_metric(
        &mut out,
        "stratumbee_confirmed_blocks",
        snapshot.confirmed_blocks,
    );
    push_metric(
        &mut out,
        "stratumbee_matured_blocks",
        snapshot.matured_blocks,
    );
    push_metric(
        &mut out,
        "stratumbee_last_gbt_success_age_seconds",
        state
            .template_engine
            .last_gbt_success_age_secs()
            .unwrap_or(u64::MAX),
    );
    push_metric(&mut out, "stratumbee_jobs_sent_total", c.jobs_sent());
    push_metric(
        &mut out,
        "stratumbee_clean_jobs_sent_total",
        c.clean_jobs_sent(),
    );
    push_metric(
        &mut out,
        "stratumbee_notify_deduped_total",
        c.notify_deduped(),
    );
    push_metric(
        &mut out,
        "stratumbee_notify_rate_limited_total",
        c.notify_rate_limited(),
    );
    push_metric(
        &mut out,
        "stratumbee_duplicate_shares_total",
        c.duplicate_shares(),
    );
    push_metric(
        &mut out,
        "stratumbee_reconnects_total",
        c.reconnects_total(),
    );
    push_metric(
        &mut out,
        "stratumbee_submitblock_accepted_total",
        c.submitblock_accepted(),
    );
    push_metric(
        &mut out,
        "stratumbee_submitblock_rejected_total",
        c.submitblock_rejected(),
    );
    push_metric(
        &mut out,
        "stratumbee_submitblock_rpc_fail_total",
        c.submitblock_rpc_fail(),
    );
    push_metric(
        &mut out,
        "stratumbee_version_rolling_violations_total",
        c.version_rolling_violations(),
    );
    push_metric(
        &mut out,
        "stratumbee_stales_new_block_total",
        c.stales_new_block(),
    );
    push_metric(
        &mut out,
        "stratumbee_stales_expired_total",
        c.stales_expired(),
    );
    push_metric(
        &mut out,
        "stratumbee_stales_reconnect_total",
        c.stales_reconnect(),
    );
    push_metric(
        &mut out,
        "stratumbee_zmq_block_notifications_total",
        c.zmq_block_received(),
    );
    push_metric(
        &mut out,
        "stratumbee_zmq_blocks_detected_total",
        c.zmq_blocks_detected(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_connections_total",
        c.sv2_connections_total(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_connections_active",
        c.sv2_connections_active(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_connections_closed_total",
        c.sv2_connections_closed(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_rejected_max_connections_total",
        c.sv2_rejected_max_connections(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_rejected_per_ip_total",
        c.sv2_rejected_per_ip(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_handshake_success_total",
        c.sv2_handshake_success(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_handshake_failures_total",
        c.sv2_handshake_failures(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_setup_accepted_total",
        c.sv2_setup_accepted(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_setup_rejected_total",
        c.sv2_setup_rejected(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_channels_opened_total",
        c.sv2_channels_opened(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_channel_open_errors_total",
        c.sv2_channel_open_errors(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_channels_closed_total",
        c.sv2_channels_closed(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_unsupported_messages_total",
        c.sv2_unsupported_messages(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_submit_accepted_total",
        c.sv2_submit_accepted(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_submit_rejected_total",
        c.sv2_submit_rejected(),
    );
    push_metric(
        &mut out,
        "stratumbee_sv2_blocks_found_total",
        c.sv2_blocks_found(),
    );

    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        out,
    )
}

fn push_metric<T: std::fmt::Display>(out: &mut String, name: &str, value: T) {
    let _ = writeln!(out, "{name} {value}");
}

#[cfg(test)]
mod tests {
    use super::{
        encode_sv2_authority_public_key, public_miner_id, public_miner_label,
        template_matches_core_tip,
    };
    use crate::metrics::MinerStats;
    use crate::template::{JobTemplate, TemplateSource};
    use chrono::Utc;

    #[test]
    fn encodes_sv2_authority_public_key_with_url_scheme_version() {
        let public_key = [0x11; 32];

        let encoded = encode_sv2_authority_public_key(&public_key);
        let decoded = bitcoin::base58::decode_check(&encoded).expect("decode sv2 authority key");

        assert_eq!(decoded.len(), 34);
        assert_eq!(&decoded[..2], &[1, 0]);
        assert_eq!(&decoded[2..], &public_key);
    }

    #[test]
    fn health_tip_match_allows_only_the_fast_job_one_height_exception() {
        let mut job = JobTemplate::empty();
        job.height = 101;
        assert!(template_matches_core_tip(&job, 100));

        job.height = 102;
        assert!(!template_matches_core_tip(&job, 100));

        job.template_source = TemplateSource::P2pCmpctFast;
        assert!(template_matches_core_tip(&job, 100));

        job.height = 103;
        assert!(!template_matches_core_tip(&job, 100));
    }

    #[test]
    fn sv2_public_miner_id_does_not_expose_worker_identity() {
        let miner = MinerStats {
            worker: "bc1qaddress.worker".to_string(),
            difficulty: 10_000.0,
            best_difficulty: 0.0,
            best_submitted_difficulty: 0.0,
            shares: 0,
            rejected: 0,
            stale: 0,
            hashrate_gh: 0.0,
            last_seen: Utc::now(),
            notify_to_submit_ms: 0.0,
            submit_rtt_ms: 0.0,
            last_share_time: None,
            user_agent: Some("Bitaxe".to_string()),
            protocol: Some("SV2".to_string()),
            session_id: Some("sv2-1".to_string()),
        };

        assert_eq!(public_miner_id(&miner), "sv2-1");
    }

    #[test]
    fn sv2_public_miner_label_uses_worker_label_without_exposing_network_address() {
        let miner = MinerStats {
            worker: "Bitaxe #69aabbcc".to_string(),
            difficulty: 10_000.0,
            best_difficulty: 0.0,
            best_submitted_difficulty: 0.0,
            shares: 0,
            rejected: 0,
            stale: 0,
            hashrate_gh: 0.0,
            last_seen: Utc::now(),
            notify_to_submit_ms: 0.0,
            submit_rtt_ms: 0.0,
            last_share_time: None,
            user_agent: Some("Bitaxe".to_string()),
            protocol: Some("SV2".to_string()),
            session_id: Some("sv2-69aabbcc".to_string()),
        };

        assert_eq!(public_miner_label(&miner), "SV2 / Bitaxe #69aabbcc");
    }
}
