use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tracing::{debug, warn};

use crate::metrics::MetricsStore;
use crate::rpc::RpcClient;

const BLOCK_STATUS_POLL_SECS: u64 = 15;

pub struct BlockMonitor {
    rpc: RpcClient,
    metrics: MetricsStore,
}

#[derive(Debug, Deserialize)]
struct BlockHeaderStatus {
    confirmations: i64,
}

impl BlockMonitor {
    pub fn new(rpc: RpcClient, metrics: MetricsStore) -> Self {
        Self { rpc, metrics }
    }

    pub async fn run(self) {
        let mut interval = tokio::time::interval(Duration::from_secs(BLOCK_STATUS_POLL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            self.refresh().await;
        }
    }

    async fn refresh(&self) {
        for hash in self.metrics.blocks_for_confirmation().await {
            let result = self
                .rpc
                .call::<BlockHeaderStatus>("getblockheader", json!([&hash, true]))
                .await;
            match result {
                Ok(header) => {
                    self.metrics
                        .update_block_chain_state(&hash, header.confirmations)
                        .await;
                }
                Err(err) => {
                    let message = err.to_string();
                    if message.contains("(-5)") || message.contains("Block not found") {
                        debug!(hash, "submitted block header is not available yet");
                    } else {
                        warn!(hash, "failed to refresh submitted block status: {err:?}");
                    }
                }
            }
        }
    }
}
