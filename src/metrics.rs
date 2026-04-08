use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use tokio::sync::RwLock;

const HASHES_PER_DIFF: f64 = 4_294_967_296.0;
const SHARE_CACHE_SIZE: usize = 40;

#[derive(Debug, Clone, Serialize)]
pub struct MinerStats {
    pub worker: String,
    pub difficulty: f64,
    pub best_difficulty: f64,
    pub best_submitted_difficulty: f64,
    pub shares: u64,
    pub rejected: u64,
    pub stale: u64,
    pub hashrate_gh: f64,
    pub last_seen: DateTime<Utc>,
    pub notify_to_submit_ms: f64,
    pub submit_rtt_ms: f64,
    pub last_share_time: Option<DateTime<Utc>>,
    pub user_agent: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShareEvent {
    pub worker: String,
    pub difficulty: f64,
    pub accepted: bool,
    pub is_block: bool,
    pub created_at: DateTime<Utc>,
    pub job_age_secs: u64,
    pub notify_delay_ms: u64,
    pub reconnect_recent: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockEvent {
    pub height: u64,
    pub hash: String,
    pub worker: String,
    pub difficulty: f64,
    pub status: String,
    pub reason: Option<String>,
    pub archive_path: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub miners: Vec<MinerStats>,
    pub total_hashrate_gh: f64,
    pub total_blocks: u64,
    pub global_best_difficulty: f64,
    pub global_best_worker: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    NewBlock,
    Expired,
    Reconnect,
}

#[derive(Default)]
pub struct PoolCounters {
    pub jobs_sent: AtomicU64,
    pub clean_jobs_sent: AtomicU64,
    pub notify_deduped: AtomicU64,
    pub notify_rate_limited: AtomicU64,
    pub duplicate_shares: AtomicU64,
    pub reconnects_total: AtomicU64,
    pub submitblock_accepted: AtomicU64,
    pub submitblock_rejected: AtomicU64,
    pub submitblock_rpc_fail: AtomicU64,
    pub version_rolling_violations: AtomicU64,
    pub stales_new_block: AtomicU64,
    pub stales_expired: AtomicU64,
    pub stales_reconnect: AtomicU64,
    pub zmq_block_received: AtomicU64,
    pub zmq_blocks_detected: AtomicU64,
}

impl PoolCounters {
    pub fn jobs_sent(&self) -> u64 {
        self.jobs_sent.load(Ordering::Relaxed)
    }
    pub fn clean_jobs_sent(&self) -> u64 {
        self.clean_jobs_sent.load(Ordering::Relaxed)
    }
    pub fn notify_deduped(&self) -> u64 {
        self.notify_deduped.load(Ordering::Relaxed)
    }
    pub fn notify_rate_limited(&self) -> u64 {
        self.notify_rate_limited.load(Ordering::Relaxed)
    }
    pub fn duplicate_shares(&self) -> u64 {
        self.duplicate_shares.load(Ordering::Relaxed)
    }
    pub fn reconnects_total(&self) -> u64 {
        self.reconnects_total.load(Ordering::Relaxed)
    }
    pub fn submitblock_accepted(&self) -> u64 {
        self.submitblock_accepted.load(Ordering::Relaxed)
    }
    pub fn submitblock_rejected(&self) -> u64 {
        self.submitblock_rejected.load(Ordering::Relaxed)
    }
    pub fn submitblock_rpc_fail(&self) -> u64 {
        self.submitblock_rpc_fail.load(Ordering::Relaxed)
    }
    pub fn zmq_block_received(&self) -> u64 {
        self.zmq_block_received.load(Ordering::Relaxed)
    }
    pub fn zmq_blocks_detected(&self) -> u64 {
        self.zmq_blocks_detected.load(Ordering::Relaxed)
    }

    pub fn inc_jobs_sent(&self, clean: bool) {
        self.jobs_sent.fetch_add(1, Ordering::Relaxed);
        if clean {
            self.clean_jobs_sent.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn inc_notify_deduped(&self) {
        self.notify_deduped.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_notify_rate_limited(&self) {
        self.notify_rate_limited.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_duplicate_share(&self) {
        self.duplicate_shares.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_reconnect(&self) {
        self.reconnects_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_submitblock_accepted(&self) {
        self.submitblock_accepted.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_submitblock_rejected(&self) {
        self.submitblock_rejected.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_submitblock_rpc_fail(&self) {
        self.submitblock_rpc_fail.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_version_rolling_violation(&self) {
        self.version_rolling_violations.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_stale(&self, reason: StaleReason) {
        match reason {
            StaleReason::NewBlock => self.stales_new_block.fetch_add(1, Ordering::Relaxed),
            StaleReason::Expired => self.stales_expired.fetch_add(1, Ordering::Relaxed),
            StaleReason::Reconnect => self.stales_reconnect.fetch_add(1, Ordering::Relaxed),
        };
    }
    pub fn inc_zmq_block_received(&self) {
        self.zmq_block_received.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_zmq_blocks_detected(&self) {
        self.zmq_blocks_detected.fetch_add(1, Ordering::Relaxed);
    }
}

struct MetricsState {
    miners: HashMap<String, MinerStats>,
    events: VecDeque<ShareEvent>,
    blocks: VecDeque<BlockEvent>,
    share_samples: HashMap<String, VecDeque<ShareSample>>,
    global_best_difficulty: f64,
    global_best_worker: Option<String>,
    max_events: usize,
    max_blocks: usize,
}

#[derive(Clone)]
pub struct MetricsStore {
    inner: Arc<RwLock<MetricsState>>,
    miner_inactive_timeout: Duration,
    pub counters: Arc<PoolCounters>,
    pub started_at: DateTime<Utc>,
}

impl MetricsStore {
    pub fn new(max_events: usize, max_blocks: usize, miner_inactive_timeout_secs: u64) -> Self {
        Self {
            inner: Arc::new(RwLock::new(MetricsState {
                miners: HashMap::new(),
                events: VecDeque::with_capacity(max_events),
                blocks: VecDeque::with_capacity(max_blocks),
                share_samples: HashMap::new(),
                global_best_difficulty: 0.0,
                global_best_worker: None,
                max_events,
                max_blocks,
            })),
            miner_inactive_timeout: Duration::seconds(miner_inactive_timeout_secs as i64),
            counters: Arc::new(PoolCounters::default()),
            started_at: Utc::now(),
        }
    }

    fn prune_inactive(&self, guard: &mut MetricsState, now: DateTime<Utc>) {
        let stale_workers = guard
            .miners
            .iter()
            .filter_map(|(worker, stats)| {
                ((now - stats.last_seen) > self.miner_inactive_timeout).then(|| worker.clone())
            })
            .collect::<Vec<_>>();

        for worker in stale_workers {
            guard.miners.remove(&worker);
            guard.share_samples.remove(&worker);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record_share(
        &self,
        worker: &str,
        target_difficulty: f64,
        share_difficulty: f64,
        accepted: bool,
        is_block: bool,
        notify_to_submit_ms: i64,
        submit_rtt_ms: f64,
        job_age_secs: u64,
        notify_delay_ms: u64,
        reconnect_recent: bool,
    ) {
        let now = Utc::now();
        let mut guard = self.inner.write().await;
        self.prune_inactive(&mut guard, now);
        let worker_name = worker.to_string();
        let should_update_global_best =
            accepted && share_difficulty > guard.global_best_difficulty;

        let mut hashrate_update = None;
        if accepted {
            let samples = guard
                .share_samples
                .entry(worker_name.clone())
                .or_insert_with(VecDeque::new);
            samples.push_back(ShareSample {
                time: now,
                difficulty: target_difficulty,
            });
            if samples.len() > SHARE_CACHE_SIZE {
                samples.pop_front();
            }
            if samples.len() >= 2 {
                let first = samples.front().unwrap();
                let last = samples.back().unwrap();
                let window_ms = (last.time - first.time).num_milliseconds().max(1) as f64;
                let sum_diff: f64 = samples.iter().skip(1).map(|s| s.difficulty).sum();
                let window_sec = window_ms / 1000.0;
                hashrate_update = Some((sum_diff * HASHES_PER_DIFF) / window_sec / 1_000_000_000.0);
            }
        }

        {
            let stats = guard.miners.entry(worker_name.clone()).or_insert_with(|| MinerStats {
                worker: worker_name.clone(),
                difficulty: target_difficulty,
                best_difficulty: 0.0,
                best_submitted_difficulty: 0.0,
                shares: 0,
                rejected: 0,
                stale: 0,
                hashrate_gh: 0.0,
                last_seen: now,
                notify_to_submit_ms: notify_to_submit_ms as f64,
                submit_rtt_ms,
                last_share_time: None,
                user_agent: None,
                session_id: None,
            });

            stats.difficulty = target_difficulty;
            stats.last_seen = now;
            if share_difficulty > stats.best_submitted_difficulty {
                stats.best_submitted_difficulty = share_difficulty;
            }
            if accepted {
                stats.shares += 1;
                if share_difficulty > stats.best_difficulty {
                    stats.best_difficulty = share_difficulty;
                }
                if let Some(hashrate) = hashrate_update {
                    stats.hashrate_gh = hashrate;
                }
                stats.last_share_time = Some(now);
            } else {
                stats.rejected += 1;
            }
            stats.notify_to_submit_ms =
                (stats.notify_to_submit_ms * 0.8) + (notify_to_submit_ms as f64 * 0.2);
            stats.submit_rtt_ms = (stats.submit_rtt_ms * 0.8) + (submit_rtt_ms * 0.2);
        }

        guard.events.push_back(ShareEvent {
            worker: worker_name.clone(),
            difficulty: share_difficulty,
            accepted,
            is_block,
            created_at: now,
            job_age_secs,
            notify_delay_ms,
            reconnect_recent,
        });
        while guard.events.len() > guard.max_events {
            guard.events.pop_front();
        }

        if should_update_global_best {
            guard.global_best_difficulty = share_difficulty;
            guard.global_best_worker = Some(worker_name);
        }
    }

    pub async fn record_stale(&self, worker: &str, reason: StaleReason) {
        self.counters.inc_stale(reason);
        let now = Utc::now();
        let mut guard = self.inner.write().await;
        self.prune_inactive(&mut guard, now);
        let worker_name = worker.to_string();
        let stats = guard.miners.entry(worker_name.clone()).or_insert_with(|| MinerStats {
            worker: worker_name,
            difficulty: 0.0,
            best_difficulty: 0.0,
            best_submitted_difficulty: 0.0,
            shares: 0,
            rejected: 0,
            stale: 0,
            hashrate_gh: 0.0,
            last_seen: now,
            notify_to_submit_ms: 0.0,
            submit_rtt_ms: 0.0,
            last_share_time: None,
            user_agent: None,
            session_id: None,
        });
        stats.stale += 1;
        stats.last_seen = now;
    }

    pub async fn record_miner_seen(
        &self,
        worker: &str,
        difficulty: f64,
        user_agent: Option<String>,
        session_id: Option<String>,
    ) {
        let now = Utc::now();
        let mut guard = self.inner.write().await;
        self.prune_inactive(&mut guard, now);
        let worker_name = worker.to_string();
        let stats = guard.miners.entry(worker_name.clone()).or_insert_with(|| MinerStats {
            worker: worker_name,
            difficulty,
            best_difficulty: 0.0,
            best_submitted_difficulty: 0.0,
            shares: 0,
            rejected: 0,
            stale: 0,
            hashrate_gh: 0.0,
            last_seen: now,
            notify_to_submit_ms: 0.0,
            submit_rtt_ms: 0.0,
            last_share_time: None,
            user_agent: None,
            session_id: None,
        });
        stats.last_seen = now;
        stats.difficulty = difficulty;
        if user_agent.is_some() {
            stats.user_agent = user_agent;
        }
        if session_id.is_some() {
            stats.session_id = session_id;
        }
    }

    pub async fn upsert_block(&self, event: BlockEvent) {
        let mut guard = self.inner.write().await;
        if let Some(existing) = guard.blocks.iter_mut().find(|item| item.hash == event.hash) {
            *existing = event;
            return;
        }
        guard.blocks.push_back(event);
        while guard.blocks.len() > guard.max_blocks {
            guard.blocks.pop_front();
        }
    }

    pub async fn snapshot(&self) -> MetricsSnapshot {
        let now = Utc::now();
        let mut guard = self.inner.write().await;
        self.prune_inactive(&mut guard, now);
        let mut miners = guard.miners.values().cloned().collect::<Vec<_>>();
        miners.sort_by(|a, b| {
            b.hashrate_gh
                .partial_cmp(&a.hashrate_gh)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let total_hashrate_gh = miners.iter().map(|m| m.hashrate_gh).sum();
        let total_blocks = guard
            .blocks
            .iter()
            .filter(|b| b.status == "submitted" || b.status == "duplicate")
            .count() as u64;

        MetricsSnapshot {
            miners,
            total_hashrate_gh,
            total_blocks,
            global_best_difficulty: guard.global_best_difficulty,
            global_best_worker: guard.global_best_worker.clone(),
        }
    }

    pub async fn recent_events(&self, window: Duration) -> Vec<ShareEvent> {
        let guard = self.inner.read().await;
        let cutoff = Utc::now() - window;
        guard
            .events
            .iter()
            .filter(|event| event.created_at >= cutoff)
            .cloned()
            .collect()
    }

    pub async fn recent_blocks(&self) -> Vec<BlockEvent> {
        let guard = self.inner.read().await;
        let mut out = guard.blocks.iter().cloned().collect::<Vec<_>>();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out
    }
}

#[derive(Debug, Clone)]
struct ShareSample {
    time: DateTime<Utc>,
    difficulty: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_prunes_inactive_miners_but_keeps_online_workers_only() {
        let store = MetricsStore::new(16, 8, 30);

        store
            .record_miner_seen("stale-worker", 64.0, None, None)
            .await;
        store
            .record_share("stale-worker", 64.0, 5_000.0, true, false, 0, 0.0, 0, 0, false)
            .await;
        store
            .record_share("stale-worker", 64.0, 0.0, false, false, 0, 0.0, 0, 0, false)
            .await;
        store
            .record_stale("stale-worker", StaleReason::Expired)
            .await;
        store
            .record_miner_seen("active-worker", 64.0, None, None)
            .await;

        let now = Utc::now();
        let mut guard = store.inner.write().await;
        guard.miners.get_mut("stale-worker").unwrap().last_seen = now - Duration::seconds(120);
        guard.miners.get_mut("active-worker").unwrap().last_seen = now;
        drop(guard);

        let snapshot = store.snapshot().await;

        assert_eq!(snapshot.miners.len(), 1);
        assert_eq!(snapshot.miners[0].worker, "active-worker");
        assert_eq!(snapshot.global_best_difficulty, 5_000.0);
        assert_eq!(snapshot.global_best_worker.as_deref(), Some("stale-worker"));

        let guard = store.inner.read().await;
        assert!(!guard.miners.contains_key("stale-worker"));
        assert!(!guard.share_samples.contains_key("stale-worker"));
    }

    #[tokio::test]
    async fn global_best_tracks_only_higher_accepted_shares() {
        let store = MetricsStore::new(16, 8, 300);

        store
            .record_share("worker-a", 64.0, 100.0, false, false, 0, 0.0, 0, 0, false)
            .await;
        store
            .record_share("worker-a", 64.0, 150.0, true, false, 0, 0.0, 0, 0, false)
            .await;
        store
            .record_share("worker-b", 64.0, 150.0, true, false, 0, 0.0, 0, 0, false)
            .await;
        store
            .record_share("worker-c", 64.0, 120.0, true, false, 0, 0.0, 0, 0, false)
            .await;
        store
            .record_share("worker-d", 64.0, 200.0, true, false, 0, 0.0, 0, 0, false)
            .await;

        let snapshot = store.snapshot().await;

        assert_eq!(snapshot.global_best_difficulty, 200.0);
        assert_eq!(snapshot.global_best_worker.as_deref(), Some("worker-d"));
    }
}
