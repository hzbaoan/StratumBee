mod api;
mod config;
mod hash;
mod metrics;
mod p2p;
mod rpc;
mod share;
mod stratum;
mod template;
mod vardiff;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tracing::{error, info};

use crate::api::ApiServer;
use crate::config::Config;
use crate::metrics::MetricsStore;
use crate::p2p::P2pClient;
use crate::rpc::RpcClient;
use crate::stratum::{MinerLatencyRegistry, StratumServer};
use crate::template::TemplateEngine;

#[derive(Parser, Debug)]
#[command(name = "stratumbee")]
#[command(about = "Solo Bitcoin template collector + Stratum V1 broadcaster")]
struct Cli {
    #[arg(short, long, default_value = "config/stratumbee.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .init();

    if let Err(err) = run(cli).await {
        error!("fatal: {err:?}");
        return Err(err);
    }

    Ok(())
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let config = Config::load(&cli.config).context("load config")?;
    let metrics = MetricsStore::new(
        config.max_recent_events,
        config.max_block_history,
        config.miner_inactive_timeout_secs,
    );
    let rpc = RpcClient::new(
        config.rpc_url.clone(),
        config.rpc_user.clone(),
        config.rpc_pass.clone(),
    )
    .context("build RPC client")?;
    let template_engine = Arc::new(TemplateEngine::new(
        config.clone(),
        rpc.clone(),
        metrics.counters.clone(),
    ));
    let latency_registry = MinerLatencyRegistry::new();

    info!(
        "booting stratumbee network={:?} stratum={}:{} api={}:{}",
        config.network,
        config.stratum_bind,
        config.stratum_port,
        config.api_bind,
        config.api_port
    );

    template_engine.start().await.context("start template engine")?;

    if config.p2p_fast_peer.is_some() {
        let p2p = P2pClient::new(config.clone(), template_engine.clone());
        tokio::spawn(async move {
            p2p.run().await;
        });
    }

    let stratum = StratumServer::new(
        config.clone(),
        template_engine.clone(),
        metrics.clone(),
        latency_registry.clone(),
    );
    let stratum_handle = tokio::spawn(async move { stratum.run().await });

    let api_handle = if config.api_enabled {
        let api = ApiServer::new(
            config.clone(),
            metrics.clone(),
            rpc.clone(),
            template_engine,
            latency_registry,
        );
        Some(tokio::spawn(async move { api.run().await }))
    } else {
        None
    };

    match api_handle {
        Some(api_handle) => {
            tokio::select! {
                res = stratum_handle => match res {
                    Ok(Ok(())) => info!("stratum stopped"),
                    Ok(Err(err)) => return Err(err).context("stratum exited with error"),
                    Err(err) => return Err(err).context("stratum task join failed"),
                },
                res = api_handle => match res {
                    Ok(Ok(())) => info!("api stopped"),
                    Ok(Err(err)) => return Err(err).context("api exited with error"),
                    Err(err) => return Err(err).context("api task join failed"),
                },
                _ = tokio::signal::ctrl_c() => {
                    info!("shutdown signal received");
                }
            }
        }
        None => {
            tokio::select! {
                res = stratum_handle => match res {
                    Ok(Ok(())) => info!("stratum stopped"),
                    Ok(Err(err)) => return Err(err).context("stratum exited with error"),
                    Err(err) => return Err(err).context("stratum task join failed"),
                },
                _ = tokio::signal::ctrl_c() => {
                    info!("shutdown signal received");
                }
            }
        }
    }

    Ok(())
}
