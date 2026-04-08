use std::sync::Arc;

use axum::extract::State;
use axum::http::{header::CONTENT_TYPE, Method, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use bitcoin::Network;
use chrono::{Duration, Utc};
use serde::Serialize;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::config::Config;
use crate::metrics::{MetricsStore, ShareEvent};
use crate::rpc::RpcClient;
use crate::stratum::MinerLatencyRegistry;
use crate::template::{JobTemplate, TemplateEngine};

const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");
const GATEWAY_VERSION: &str = concat!("StratumBee ", env!("CARGO_PKG_VERSION"));

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
}

async fn index() -> impl IntoResponse {
    Html(DASHBOARD_HTML)
}

async fn health(State(state): State<ApiState>) -> impl IntoResponse {
    let job = state.template_engine.current_job().await;
    let ok = job.ready;
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(serde_json::json!({
        "ok": ok,
        "template_ready": job.ready,
        "uptime_secs": (Utc::now() - state.metrics.started_at).num_seconds().max(0),
    })))
}

#[derive(Serialize)]
struct SummaryResponse {
    uptime_secs: u64,
    total_hashrate_gh: f64,
    total_blocks: u64,
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
}

async fn summary(State(state): State<ApiState>) -> impl IntoResponse {
    let snapshot = state.metrics.snapshot().await;
    let job = state.template_engine.current_job().await;
    let c = &state.metrics.counters;
    Json(SummaryResponse {
        uptime_secs: (Utc::now() - state.metrics.started_at).num_seconds().max(0) as u64,
        total_hashrate_gh: snapshot.total_hashrate_gh,
        total_blocks: snapshot.total_blocks,
        connected_miners: snapshot.miners.len(),
        current_height: job.height,
        current_reward_sat: job.coinbase_value,
        current_tx_count: job.transactions.len(),
        network_difficulty: job.network_difficulty,
        current_prevhash: display_prevhash(job.as_ref()),
        global_best_diff: snapshot.global_best_difficulty,
        global_best_diff_worker: snapshot.global_best_worker.clone(),
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
    })
}

async fn miners(State(state): State<ApiState>) -> impl IntoResponse {
    Json(state.metrics.snapshot().await.miners)
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
        template_ready: job.ready,
    })
}

#[derive(Serialize)]
struct WorkerResponse {
    user_agent: String,
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
            let display_name = miner.worker.clone();
            let user_agent = miner
                .user_agent
                .clone()
                .unwrap_or_else(|| display_name.clone());

            WorkerResponse {
                user_agent,
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

async fn blocks(State(state): State<ApiState>) -> impl IntoResponse {
    Json(state.metrics.recent_blocks().await)
}

async fn events(State(state): State<ApiState>) -> impl IntoResponse {
    let events: Vec<ShareEvent> = state.metrics.recent_events(Duration::minutes(30)).await;
    Json(events)
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
        blocks: info.as_ref().and_then(|v| v.get("blocks")).and_then(|v| v.as_u64()),
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
    Json(state.latency_registry.sample_all().await)
}
