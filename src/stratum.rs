use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::Config;
use crate::metrics::{BlockEvent, MetricsStore, StaleReason};
use crate::share::{share_target_le, validate_share, ShareSubmit};
use crate::template::{JobTemplate, SubmitBlockOutcome, TemplateEngine, TemplateSource};
use crate::vardiff::VardiffController;

type DupKey = (u32, u32, u32, [u8; 16], u8, u32);

const SESSION_JOB_ID_HEX_LEN: usize = 8;

#[derive(Clone)]
pub struct StratumServer {
    config: Config,
    template_engine: Arc<TemplateEngine>,
    metrics: MetricsStore,
}

impl StratumServer {
    pub fn new(config: Config, template_engine: Arc<TemplateEngine>, metrics: MetricsStore) -> Self {
        Self {
            config,
            template_engine,
            metrics,
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let bind = format!("{}:{}", self.config.stratum_bind, self.config.stratum_port);
        let listener = TcpListener::bind(&bind).await?;
        info!("stratum listening on {bind}");

        loop {
            let (stream, addr) = listener.accept().await?;
            let server = self.clone();
            tokio::spawn(async move {
                if let Err(err) = server.handle_client(stream, addr).await {
                    warn!("client {addr} error: {err:?}");
                }
            });
        }
    }

    async fn handle_client(&self, stream: TcpStream, addr: SocketAddr) -> anyhow::Result<()> {
        let _ = stream.set_nodelay(true);
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let (tx, mut rx) = mpsc::channel::<String>(64);
        let writer_task = tokio::spawn(async move {
            let mut write_buf = Vec::with_capacity(512);
            while let Some(msg) = rx.recv().await {
                write_buf.clear();
                write_buf.extend_from_slice(msg.as_bytes());
                write_buf.push(b'\n');
                if writer.write_all(&write_buf).await.is_err() {
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });

        let mut extranonce1 = vec![0u8; self.config.extranonce1_size];
        rand::thread_rng().fill_bytes(&mut extranonce1);
        let extranonce1_hex = hex::encode(&extranonce1);

        let state = Arc::new(Mutex::new(SessionState::new(
            extranonce1_hex.clone(),
            extranonce1.clone(),
            self.config.start_difficulty,
            self.config.target_share_time_secs,
            self.config.vardiff_retarget_time_secs,
            self.config.min_difficulty,
            self.config.max_difficulty,
            self.config.vardiff_enabled,
            self.config.notify_bucket_capacity,
            self.config.notify_bucket_refill_ms,
        )));

        let mut notify_rx = self.template_engine.subscribe();
        let notify_tx = tx.clone();
        let notify_state = state.clone();
        let notify_counters = self.metrics.counters.clone();
        let notify_metrics = self.metrics.clone();

        let notify_task = tokio::spawn(async move {
            'notify: loop {
                tokio::select! {
                    _ = notify_tx.closed() => break,
                    changed = notify_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                }
                loop {
                    let job = notify_rx.borrow().clone();
                    if !job.ready {
                        break;
                    }

                    enum NotifyAction {
                        Skip,
                        WaitForBootstrap,
                        Send {
                            dispatch: NotifyDispatch,
                            post_send: NotifyPostSend,
                        },
                    }

                    let action = {
                        let mut guard = notify_state.lock().await;
                        if !guard.authorized {
                            NotifyAction::Skip
                        } else if !guard.bootstrap_complete && guard.bootstrap_inflight {
                            NotifyAction::WaitForBootstrap
                        } else {
                            let bootstrap_notify = !guard.bootstrap_complete;
                            if bootstrap_notify {
                                let claimed = guard.claim_bootstrap_notify();
                                debug_assert!(claimed, "bootstrap notify should be claimable once");
                            }
                            let clean_jobs = if bootstrap_notify {
                                true
                            } else {
                                guard.should_clean_jobs(job.as_ref())
                            };

                            if !bootstrap_notify && !clean_jobs && !guard.notify_bucket.try_consume() {
                                notify_counters.inc_notify_rate_limited();
                                NotifyAction::Skip
                            } else {
                                if clean_jobs {
                                    guard.mark_jobs_stale_block();
                                }
                                let diff = guard.difficulty;
                                let extranonce1_bytes = guard.extranonce1_bytes.clone();
                                let session_job =
                                    guard.push_job(job.clone(), diff, &extranonce1_bytes, None);
                                let trigger = if bootstrap_notify {
                                    "template_update_bootstrap"
                                } else {
                                    "template_update"
                                };
                                NotifyAction::Send {
                                    dispatch: build_notify_dispatch(
                                        &guard.worker,
                                        diff,
                                        session_job.as_ref(),
                                        clean_jobs,
                                        trigger,
                                    ),
                                    post_send: NotifyPostSend::new(
                                        job.prevhash_le.clone(),
                                        clean_jobs,
                                        bootstrap_notify,
                                    ),
                                }
                            }
                        }
                    };

                    match action {
                        NotifyAction::Skip => break,
                        NotifyAction::WaitForBootstrap => {
                            if notify_tx.is_closed() {
                                break 'notify;
                            }
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        NotifyAction::Send { dispatch, post_send } => {
                            let trace = match send_notify(&notify_tx, dispatch).await {
                                Ok(trace) => trace,
                                Err(_) => break 'notify,
                            };
                            {
                                let mut guard = notify_state.lock().await;
                                guard.note_notify_sent(post_send);
                            }
                            notify_counters.inc_jobs_sent(trace.clean_jobs);
                            if !trace.miner.is_empty() {
                                notify_metrics
                                    .record_miner_seen(&trace.miner, trace.difficulty, None, None)
                                    .await;
                            }
                            break;
                        }
                    }
                }
            }
        });

        let vardiff_state = state.clone();
        let vardiff_tx = tx.clone();
        let vardiff_config = self.config.clone();
        let vardiff_metrics = self.metrics.clone();
        let vardiff_task = tokio::spawn(async move {
            if !vardiff_config.vardiff_enabled {
                return;
            }
            let interval_ms = (vardiff_config.vardiff_retarget_time_secs * 1000.0).max(1000.0) as u64;
            let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
            loop {
                tokio::select! {
                    _ = vardiff_tx.closed() => break,
                    _ = interval.tick() => {}
                }
                let now = Utc::now();
                let (diff_msg, notify_msg) = {
                    let mut guard = vardiff_state.lock().await;
                    if !guard.authorized || !guard.bootstrap_complete {
                        (None, None)
                    } else {
                        let current_diff = guard.difficulty;
                        if let Some(new_diff) = guard.vardiff.maybe_retarget(current_diff, now) {
                            guard.set_difficulty(new_diff);
                            let diff_msg = Some(build_set_difficulty(new_diff));
                            let notify_msg = if let Some(job_latest) = guard.jobs.back().map(|e| e.job.clone()) {
                                let extranonce1_bytes = guard.extranonce1_bytes.clone();
                                let session_job =
                                    guard.push_job(job_latest.clone(), new_diff, &extranonce1_bytes, None);
                                Some((
                                    build_notify_dispatch(
                                        &guard.worker,
                                        new_diff,
                                        session_job.as_ref(),
                                        false,
                                        "vardiff_retarget",
                                    ),
                                    NotifyPostSend::new(job_latest.prevhash_le.clone(), false, false),
                                ))
                            } else {
                                None
                            };
                            (diff_msg, notify_msg)
                        } else {
                            (None, None)
                        }
                    }
                };
                if let Some(msg) = diff_msg {
                    if vardiff_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                if let Some((msg, post_send)) = notify_msg {
                    let trace = match send_notify(&vardiff_tx, msg).await {
                        Ok(trace) => trace,
                        Err(_) => break,
                    };
                    {
                        let mut guard = vardiff_state.lock().await;
                        guard.note_notify_sent(post_send);
                    }
                    if !trace.miner.is_empty() {
                        vardiff_metrics
                            .record_miner_seen(&trace.miner, trace.difficulty, None, None)
                            .await;
                    }
                }
            }
        });

        let session_tasks = SessionTasks::new(tx, writer_task, notify_task, vardiff_task);

        info!("miner connected: {addr}");
        self.metrics.counters.inc_reconnect();

        let tx = session_tasks.tx.clone();
        let run_result: anyhow::Result<()> = async move {
            let mut buf = Vec::with_capacity(512);
            loop {
                buf.clear();
                let read_result = tokio::time::timeout(
                    Duration::from_secs(self.config.idle_timeout_secs),
                    reader.read_until(b'\n', &mut buf),
                )
                .await;

                let n = match read_result {
                    Err(_) => {
                        warn!("idle timeout for {addr}, closing session");
                        break;
                    }
                    Ok(Err(io_err)) => return Err(io_err.into()),
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => n,
                };

                if n > self.config.max_line_bytes {
                    warn!(
                        "oversized Stratum line from {addr}: {} bytes > {}",
                        n,
                        self.config.max_line_bytes
                    );
                    break;
                }

                let line = match std::str::from_utf8(&buf[..n]) {
                    Ok(s) => s.trim_end_matches(|c| c == '\n' || c == '\r'),
                    Err(_) => {
                        warn!("non-UTF8 line from {addr}; skipping");
                        continue;
                    }
                };
                if line.is_empty() {
                    continue;
                }

                let request: StratumRequest = match serde_json::from_str(line) {
                    Ok(req) => req,
                    Err(err) => {
                        warn!("invalid json from {addr}: {err}");
                        continue;
                    }
                };

                match request.method.as_str() {
                    "mining.subscribe" => {
                        let user_agent = request
                            .params
                            .as_array()
                            .and_then(|params| params.first())
                            .and_then(|value| value.as_str())
                            .and_then(normalize_user_agent);
                        if user_agent.is_some() {
                            let mut guard = state.lock().await;
                            guard.user_agent = user_agent;
                        }
                        let response = json!({
                            "id": request.id.clone(),
                            "result": [
                                [
                                    ["mining.set_difficulty", "1"],
                                    ["mining.notify", "1"]
                                ],
                                extranonce1_hex,
                                self.config.extranonce2_size
                            ],
                            "error": null
                        });
                        let _ = tx.send(response.to_string()).await;
                    }
                    "mining.extranonce.subscribe" => {
                        let _ = tx
                            .send(json!({"id": request.id.clone(), "result": true, "error": null}).to_string())
                            .await;
                    }
                    "mining.authorize" => {
                        let authorized = self.handle_authorize(&request).await;
                        let _ = tx
                            .send(json!({"id": request.id.clone(), "result": authorized, "error": null}).to_string())
                            .await;

                        if authorized {
                            let (worker, difficulty, user_agent, extranonce1, diff_msg) = {
                                let mut guard = state.lock().await;
                                guard.begin_authorized_session();
                                guard.bootstrap_inflight = true;

                                let difficulty = guard.difficulty;
                                let user_agent = guard.user_agent.clone();
                                let extranonce1 = guard.extranonce1.clone();
                                let worker = build_miner_label(user_agent.as_deref(), &extranonce1);
                                guard.worker = worker.clone();
                                let diff_msg = build_set_difficulty(difficulty);
                                (worker, difficulty, user_agent, extranonce1, diff_msg)
                            };

                            let _ = tx.send(diff_msg).await;
                            let current_job = self.template_engine.current_job().await;
                            let notify_opt = if current_job.ready {
                                let mut guard = state.lock().await;
                                if guard.authorized && guard.bootstrap_inflight && !guard.bootstrap_complete {
                                    let extranonce1_bytes = guard.extranonce1_bytes.clone();
                                    let session_job =
                                        guard.push_job(current_job.clone(), difficulty, &extranonce1_bytes, None);
                                    Some((
                                        build_notify_dispatch(
                                            &worker,
                                            difficulty,
                                            session_job.as_ref(),
                                            true,
                                            "authorize",
                                        ),
                                        NotifyPostSend::new(
                                            current_job.prevhash_le.clone(),
                                            true,
                                            true,
                                        ),
                                    ))
                                } else {
                                    None
                                }
                            } else {
                                let mut guard = state.lock().await;
                                if guard.authorized && guard.bootstrap_inflight && !guard.bootstrap_complete {
                                    guard.bootstrap_inflight = false;
                                }
                                None
                            };
                            if let Some((notify, post_send)) = notify_opt {
                                if send_notify(&tx, notify).await.is_ok() {
                                    let mut guard = state.lock().await;
                                    guard.note_notify_sent(post_send);
                                }
                            }

                            info!(
                                "session authorized miner={} ua={} en1={} diff={:.0}",
                                worker,
                                user_agent.as_deref().unwrap_or("unknown"),
                                extranonce1,
                                difficulty
                            );

                            self.metrics
                                .record_miner_seen(
                                    &worker,
                                    difficulty,
                                    user_agent,
                                    Some(extranonce1),
                                )
                                .await;
                        }
                    }
                    "mining.get_version" => {
                        let _ = tx
                            .send(json!({"id": request.id.clone(), "result": "StratumBee", "error": null}).to_string())
                            .await;
                    }
                    "mining.ping" => {
                        let heartbeat = {
                            let guard = state.lock().await;
                            if guard.authorized && !guard.worker.is_empty() {
                                Some((guard.worker.clone(), guard.difficulty))
                            } else {
                                None
                            }
                        };
                        let sent = tx
                            .send(json!({"id": request.id.clone(), "result": true, "error": null}).to_string())
                            .await
                            .is_ok();
                        if sent {
                            if let Some((worker, difficulty)) = heartbeat {
                                self.metrics
                                    .record_miner_seen(&worker, difficulty, None, None)
                                    .await;
                            }
                        }
                    }
                    "mining.suggest_difficulty" => {
                        self.handle_suggest_difficulty(&request, &state, &tx).await?;
                    }
                    "mining.submit" => {
                        self.handle_submit(&request, &state, &tx).await?;
                    }
                    "mining.configure" => {
                        let allowed_mask = 0x1fffe000u32;
                        let requested_extensions = request
                            .params
                            .as_array()
                            .and_then(|params| params.first())
                            .and_then(|value| value.as_array())
                            .cloned()
                            .unwrap_or_default();
                        let options = request
                            .params
                            .as_array()
                            .and_then(|params| params.get(1))
                            .and_then(|value| value.as_object());
                        let requested_mask = options
                            .and_then(|value| value.get("version-rolling.mask"))
                            .and_then(|value| value.as_str())
                            .unwrap_or("1fffe000");
                        let parsed_mask =
                            u32::from_str_radix(requested_mask.trim_start_matches("0x"), 16)
                                .unwrap_or(allowed_mask);
                        let configured_mask = parsed_mask & allowed_mask;
                        let version_rolling_requested = requested_extensions
                            .iter()
                            .any(|value| value.as_str() == Some("version-rolling"));

                        if version_rolling_requested {
                            let mut guard = state.lock().await;
                            guard.version_mask = configured_mask;
                        }
                        let mut result = serde_json::Map::new();
                        for extension in requested_extensions.iter().filter_map(|value| value.as_str()) {
                            match extension {
                                "version-rolling" => {
                                    result.insert(extension.to_string(), json!(true));
                                    result.insert(
                                        "version-rolling.mask".to_string(),
                                        json!(format!("{:08x}", configured_mask)),
                                    );
                                }
                                _ => {
                                    result.insert(extension.to_string(), json!(false));
                                }
                            }
                        }
                        let _ = tx
                            .send(json!({
                                "id": request.id.clone(),
                                "result": result,
                                "error": null
                            }).to_string())
                            .await;
                    }
                    _ => {
                        let _ = tx
                            .send(
                                json!({"id": request.id.clone(), "result": null, "error": [20, "Unsupported method", null]})
                                    .to_string(),
                            )
                            .await;
                    }
                }
            }

            Ok(())
        }
        .await;

        session_tasks.shutdown().await;
        info!("session closed: {addr}");

        run_result
    }

    async fn handle_suggest_difficulty(
        &self,
        request: &StratumRequest,
        state: &Arc<Mutex<SessionState>>,
        tx: &mpsc::Sender<String>,
    ) -> anyhow::Result<()> {
        let suggested = request
            .params
            .as_array()
            .and_then(|params| params.first())
            .and_then(|value| value.as_f64())
            .unwrap_or(self.config.start_difficulty);

        let (diff_msg, notify_msg) = {
            let mut guard = state.lock().await;
            let raw = if guard.vardiff_enabled {
                suggested
                    .clamp(self.config.min_difficulty, self.config.max_difficulty)
                    .max(guard.difficulty)
            } else {
                suggested.clamp(self.config.min_difficulty, self.config.max_difficulty)
            };
            let target = guard.vardiff.nearest_p2(raw);
            if (target - guard.difficulty).abs() > f64::EPSILON {
                guard.set_difficulty(target);
                let diff_msg = build_set_difficulty(target);
                let notify_msg = if guard.bootstrap_complete {
                    if let Some(job_latest) = guard.jobs.back().map(|e| e.job.clone()) {
                        let extranonce1_bytes = guard.extranonce1_bytes.clone();
                        let session_job =
                            guard.push_job(job_latest.clone(), target, &extranonce1_bytes, None);
                        Some((
                            build_notify_dispatch(
                                &guard.worker,
                                target,
                                session_job.as_ref(),
                                false,
                                "suggest_difficulty",
                            ),
                            NotifyPostSend::new(job_latest.prevhash_le.clone(), false, false),
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                };
                (diff_msg, notify_msg)
            } else {
                (build_set_difficulty(guard.difficulty), None)
            }
        };

        let _ = tx.send(diff_msg).await;
        if let Some((notify, post_send)) = notify_msg {
            if send_notify(tx, notify).await.is_ok() {
                let mut guard = state.lock().await;
                guard.note_notify_sent(post_send);
            }
        }
        let _ = tx
            .send(json!({"id": request.id.clone(), "result": true, "error": null}).to_string())
            .await;
        Ok(())
    }

    async fn handle_authorize(
        &self,
        request: &StratumRequest,
    ) -> bool {
        let params = request.params.as_array().cloned().unwrap_or_default();
        let password = params.get(1).and_then(|v| v.as_str()).unwrap_or("");

        self.config
            .auth_token
            .as_ref()
            .map_or(true, |token| password == token)
    }

    async fn handle_submit(
        &self,
        request: &StratumRequest,
        state: &Arc<Mutex<SessionState>>,
        tx: &mpsc::Sender<String>,
    ) -> anyhow::Result<()> {
        let submit_start = std::time::Instant::now();
        let params = request.params.as_array().cloned().unwrap_or_default();
        if params.len() < 5 {
            let _ = tx
                .send(json!({"id": request.id.clone(), "result": false, "error": [20, "Invalid params", null]}).to_string())
                .await;
            return Ok(());
        }

        let submitted_worker = params[0].as_str().unwrap_or("").to_string();
        let job_id = match params
            .get(1)
            .and_then(|value| value.as_str())
            .and_then(normalize_session_job_id)
        {
            Some(job_id) => job_id,
            None => {
                let _ = tx
                    .send(json!({"id": request.id.clone(), "result": false, "error": [20, "Invalid job_id", null]}).to_string())
                    .await;
                return Ok(());
            }
        };
        let extranonce2 = params[2].as_str().unwrap_or("").to_string();
        let ntime = params[3].as_str().unwrap_or("").to_string();
        let nonce = params[4].as_str().unwrap_or("").to_string();
        let submit_version = params
            .get(5)
            .and_then(|v| v.as_str())
            .map(|s| s.trim_start_matches("0x").to_string());

        let mut extranonce2_clean = extranonce2.trim_start_matches("0x").to_string();
        let mut ntime_clean = ntime.trim_start_matches("0x").to_string();
        let mut nonce_clean = nonce.trim_start_matches("0x").to_string();
        let expected_extra2 = self.config.extranonce2_size * 2;

        if extranonce2_clean.len() > 256 {
            let _ = tx
                .send(json!({"id": request.id.clone(), "result": false, "error": [20, "Invalid extranonce2 length", null]}).to_string())
                .await;
            return Ok(());
        }
        if extranonce2_clean.len() % 2 != 0 {
            extranonce2_clean = format!("0{extranonce2_clean}");
        }
        if extranonce2_clean.len() < expected_extra2 {
            extranonce2_clean = format!("{extranonce2_clean:0>width$}", width = expected_extra2);
        }
        if extranonce2_clean.len() != expected_extra2 {
            let _ = tx
                .send(json!({"id": request.id.clone(), "result": false, "error": [20, "Invalid extranonce2 length", null]}).to_string())
                .await;
            return Ok(());
        }

        if ntime_clean.len() % 2 != 0 {
            ntime_clean = format!("0{ntime_clean}");
        }
        if nonce_clean.len() % 2 != 0 {
            nonce_clean = format!("0{nonce_clean}");
        }
        if ntime_clean.len() < 8 {
            ntime_clean = format!("{ntime_clean:0>8}");
        }
        if nonce_clean.len() < 8 {
            nonce_clean = format!("{nonce_clean:0>8}");
        }
        if ntime_clean.len() != 8 || nonce_clean.len() != 8 {
            let _ = tx
                .send(json!({"id": request.id.clone(), "result": false, "error": [20, "Invalid ntime/nonce length", null]}).to_string())
                .await;
            return Ok(());
        }

        let (authorized, version_mask, session_job, last_notify, session_start, vardiff_enabled, session_worker, stale_reason) = {
            let guard = state.lock().await;
            if !guard.authorized {
                (
                    false,
                    guard.version_mask,
                    None,
                    guard.last_notify,
                    guard.session_start,
                    guard.vardiff_enabled,
                    guard.worker.clone(),
                    None,
                )
            } else {
                match guard.find_job(&job_id) {
                    Some(job) => (
                        true,
                        guard.version_mask,
                        Some(job),
                        guard.last_notify,
                        guard.session_start,
                        guard.vardiff_enabled,
                        guard.worker.clone(),
                        None,
                    ),
                    None => {
                        let now = Utc::now();
                        let reason = if (now - guard.session_start).num_seconds() < 30 {
                            StaleReason::Reconnect
                        } else {
                            match guard.last_clean_jobs_time {
                                Some(t) if (now - t).num_seconds() <= 60 => StaleReason::NewBlock,
                                _ => StaleReason::Expired,
                            }
                        };
                        (
                            true,
                            guard.version_mask,
                            None,
                            guard.last_notify,
                            guard.session_start,
                            guard.vardiff_enabled,
                            guard.worker.clone(),
                            Some(reason),
                        )
                    }
                }
            }
        };

        if !authorized {
            let _ = tx
                .send(json!({"id": request.id.clone(), "result": false, "error": [24, "Unauthorized", null]}).to_string())
                .await;
            return Ok(());
        }

        let session_job = match session_job {
            Some(job) => job,
            None => {
                let _ = tx
                    .send(json!({"id": request.id.clone(), "result": false, "error": [21, "Stale share", null]}).to_string())
                    .await;
                if !session_worker.is_empty() {
                    self.metrics
                        .record_stale(&session_worker, stale_reason.unwrap_or(StaleReason::Expired))
                        .await;
                }
                return Ok(());
            }
        };

        let worker = if !session_worker.is_empty() {
            session_worker
        } else {
            submitted_worker
        };
        let job = session_job.job.clone();
        let job_diff = session_job.difficulty;

        let (version, version_outside_mask) = if let Some(submit_val) =
            submit_version.as_deref().and_then(parse_u32_be)
        {
            let job_val = parse_u32_be(&job.version).unwrap_or(job.version_u32);
            let submit_outside = submit_val & !version_mask;
            let job_outside = job_val & !version_mask;
            let outside_mismatch = submit_outside != 0 && submit_outside != job_outside;
            let combined = (job_val & !version_mask) | (submit_val & version_mask);
            (Some(format!("{:08x}", combined)), outside_mismatch)
        } else {
            (None, false)
        };

        if version_outside_mask {
            self.metrics.counters.inc_version_rolling_violation();
            let _ = tx
                .send(
                    json!({"id": request.id.clone(), "result": false, "error": [20, "Version bits outside negotiated mask", null]})
                        .to_string(),
                )
                .await;
            self.metrics
                .record_share(&worker, session_job.difficulty, 0.0, false, false, 0, 0.0, 0, 0, false)
                .await;
            return Ok(());
        }

        let submit = ShareSubmit {
            extranonce2: extranonce2_clean.clone(),
            ntime: ntime_clean.clone(),
            nonce: nonce_clean.clone(),
            version: version.clone(),
        };

        const MAX_DUP_HASHES: usize = 4096;
        let version_key = version.as_deref().unwrap_or(&job.version);
        let dup_key = build_dup_key(
            &job_id,
            &nonce_clean,
            &ntime_clean,
            &extranonce2_clean,
            version_key,
        );

        let is_duplicate = {
            let mut guard = state.lock().await;
            if guard.submitted_hashes.contains(&dup_key) {
                true
            } else {
                if guard.submitted_hashes.len() >= MAX_DUP_HASHES {
                    if let Some(oldest) = guard.submitted_hashes_order.pop_front() {
                        guard.submitted_hashes.remove(&oldest);
                    }
                }
                guard.submitted_hashes.insert(dup_key);
                guard.submitted_hashes_order.push_back(dup_key);
                false
            }
        };
        if is_duplicate {
            self.metrics.counters.inc_duplicate_share();
            let _ = tx
                .send(json!({"id": request.id.clone(), "result": false, "error": [22, "Duplicate share", null]}).to_string())
                .await;
            self.metrics
                .record_share(&worker, session_job.difficulty, 0.0, false, false, 0, 0.0, 0, 0, false)
                .await;
            return Ok(());
        }

        let result = match validate_share(
            session_job.job.as_ref(),
            session_job.coinbase_prefix.as_slice(),
            &submit,
            &session_job.share_target_le,
            session_job
                .custom_coinbase2_bytes
                .as_deref()
                .map(|bytes| bytes.as_slice()),
        ) {
            Ok(value) => value,
            Err(err) => {
                let _ = tx
                    .send(
                        json!({"id": request.id.clone(), "result": false, "error": [20, format!("Invalid share params: {err}"), null]})
                            .to_string(),
                    )
                    .await;
                return Ok(());
            }
        };

        let now = Utc::now();
        let notify_to_submit_ms = (now - last_notify).num_milliseconds().max(0);
        let notify_delay_ms = notify_to_submit_ms as u64;
        let job_age_secs = (now - session_job.job.created_at).num_seconds().max(0) as u64;
        let reconnect_recent = (now - session_start).num_seconds() < 30;
        let submit_rtt_ms = submit_start.elapsed().as_micros() as f64 / 1000.0;

        let ack_error = if result.accepted {
            serde_json::Value::Null
        } else {
            json!([23, "Low difficulty share", null])
        };
        let _ = tx
            .send(json!({"id": request.id.clone(), "result": result.accepted, "error": ack_error}).to_string())
            .await;

        if result.accepted && result.is_block {
            if session_job.is_stale_block.load(Ordering::Acquire) {
                warn!(
                    "block candidate below target on stale job worker={} height={} hash={} not submitted",
                    worker,
                    job.height,
                    result.hash_hex
                );
            } else if let Some(block_hex_owned) = result.block_hex.clone() {
                let coinbase_hex_owned = result.coinbase_hex.clone().unwrap_or_default();
                let block_hash = result.hash_hex.clone();
                let template_key = job.template_key.clone();
                let txid_root = job.txid_partial_root.clone();
                let witness = job.witness_commitment_script.clone().unwrap_or_default();
                let height = job.height;
                let difficulty = result.difficulty;
                let archive_dir = self.config.save_solved_blocks_dir.clone();
                let archive_before_submit = self.config.block_archive_pre_submit;
                let engine = self.template_engine.clone();
                let metrics = self.metrics.clone();

                self.metrics
                    .upsert_block(BlockEvent {
                        height,
                        hash: block_hash.clone(),
                        worker: worker.clone(),
                        difficulty,
                        status: "candidate".to_string(),
                        reason: None,
                        archive_path: None,
                        created_at: now,
                    })
                    .await;

                info!(
                    "*** BLOCK FOUND worker={} height={} hash={} diff={:.2} ***",
                    worker,
                    height,
                    block_hash,
                    difficulty
                );

                let block_worker = worker.clone();

                tokio::spawn(async move {
                    let mut archive_path = None;
                    if archive_before_submit {
                        archive_path = maybe_archive_candidate_block(
                            archive_dir.clone(),
                            height,
                            &block_hash,
                            &block_hex_owned,
                            &coinbase_hex_owned,
                            &template_key,
                        )
                        .await;
                    }

                    let (status, reason) = match engine
                        .submit_block(
                            &block_hex_owned,
                            &block_hash,
                            &template_key,
                            &coinbase_hex_owned,
                            &txid_root,
                            &witness,
                        )
                        .await
                    {
                        Ok(SubmitBlockOutcome::Submitted) => ("submitted".to_string(), None),
                        Ok(SubmitBlockOutcome::Duplicate) => ("duplicate".to_string(), None),
                        Err(err) => ("submit_failed".to_string(), Some(err.to_string())),
                    };

                    if archive_path.is_none() {
                        archive_path = maybe_archive_candidate_block(
                            archive_dir,
                            height,
                            &block_hash,
                            &block_hex_owned,
                            &coinbase_hex_owned,
                            &template_key,
                        )
                        .await;
                    }

                    metrics
                        .upsert_block(BlockEvent {
                            height,
                            hash: block_hash,
                            worker: block_worker,
                            difficulty,
                            status,
                            reason,
                            archive_path,
                            created_at: Utc::now(),
                        })
                        .await;
                });
            }
        }

        let mut outbound_msgs = Vec::new();
        let mut outbound_notifies = Vec::new();
        {
            let mut guard = state.lock().await;
            if result.accepted {
                guard.vardiff.record_share(now, job_diff);
            }
            if vardiff_enabled && guard.bootstrap_complete {
                let current_diff = guard.difficulty;
                if let Some(new_diff) = guard.vardiff.maybe_retarget(current_diff, now) {
                    guard.set_difficulty(new_diff);
                    outbound_msgs.push(build_set_difficulty(new_diff));
                    if let Some(job_latest) = guard.jobs.back().map(|entry| entry.job.clone()) {
                        let extranonce1_bytes = guard.extranonce1_bytes.clone();
                        let session_job =
                            guard.push_job(job_latest.clone(), new_diff, &extranonce1_bytes, None);
                        outbound_notifies.push((
                            build_notify_dispatch(
                                &guard.worker,
                                new_diff,
                                session_job.as_ref(),
                                false,
                                "submit_retarget",
                            ),
                            NotifyPostSend::new(job_latest.prevhash_le.clone(), false, false),
                        ));
                    }
                }
            }
        }

        self.metrics
            .record_share(
                &worker,
                job_diff,
                result.difficulty,
                result.accepted,
                result.is_block,
                notify_to_submit_ms,
                submit_rtt_ms,
                job_age_secs,
                notify_delay_ms,
                reconnect_recent,
            )
            .await;

        for msg in outbound_msgs {
            let _ = tx.send(msg).await;
        }
        for (notify, post_send) in outbound_notifies {
            if send_notify(tx, notify).await.is_ok() {
                let mut guard = state.lock().await;
                guard.note_notify_sent(post_send);
            }
        }

        Ok(())
    }
}

async fn maybe_archive_candidate_block(
    dir: Option<PathBuf>,
    height: u64,
    hash: &str,
    block_hex: &str,
    coinbase_hex: &str,
    template_key: &str,
) -> Option<String> {
    let dir = dir?;
    match archive_candidate_block(dir, height, hash, block_hex, coinbase_hex, template_key).await {
        Ok(path) => Some(path),
        Err(err) => {
            warn!("failed to archive candidate block height={} hash={}: {err:?}", height, hash);
            None
        }
    }
}

async fn archive_candidate_block(
    dir: PathBuf,
    height: u64,
    hash: &str,
    block_hex: &str,
    coinbase_hex: &str,
    template_key: &str,
) -> anyhow::Result<String> {
    fs::create_dir_all(&dir).await?;
    let filename = format!("{}-{}.json", height, hash);
    let full_path = dir.join(filename);
    let body = json!({
        "height": height,
        "hash": hash,
        "template_key": template_key,
        "block_hex": block_hex,
        "coinbase_hex": coinbase_hex,
        "saved_at": Utc::now().to_rfc3339(),
    });
    fs::write(&full_path, serde_json::to_vec_pretty(&body)?).await?;
    Ok(full_path.display().to_string())
}

#[derive(Debug, Deserialize)]
struct StratumRequest {
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

struct NotifyBucket {
    tokens: f64,
    capacity: f64,
    fill_per_ms: f64,
    last_fill_ms: u64,
}

impl NotifyBucket {
    fn new(capacity: f64, refill_ms: f64) -> Self {
        let fill_per_ms = if refill_ms > 0.0 { 1.0 / refill_ms } else { 1.0 };
        Self {
            tokens: capacity,
            capacity,
            fill_per_ms,
            last_fill_ms: 0,
        }
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    fn try_consume(&mut self) -> bool {
        let now = Self::now_ms();
        if self.last_fill_ms == 0 {
            self.last_fill_ms = now;
        }
        let elapsed_ms = now.saturating_sub(self.last_fill_ms) as f64;
        self.tokens = (self.tokens + elapsed_ms * self.fill_per_ms).min(self.capacity);
        self.last_fill_ms = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

struct SessionTasks {
    tx: mpsc::Sender<String>,
    writer_task: JoinHandle<()>,
    notify_task: JoinHandle<()>,
    vardiff_task: JoinHandle<()>,
}

impl SessionTasks {
    fn new(
        tx: mpsc::Sender<String>,
        writer_task: JoinHandle<()>,
        notify_task: JoinHandle<()>,
        vardiff_task: JoinHandle<()>,
    ) -> Self {
        Self {
            tx,
            writer_task,
            notify_task,
            vardiff_task,
        }
    }

    async fn shutdown(self) {
        let Self {
            tx,
            writer_task,
            notify_task,
            vardiff_task,
        } = self;

        notify_task.abort();
        vardiff_task.abort();
        drop(tx);
        writer_task.abort();

        let _ = writer_task.await;
        let _ = notify_task.await;
        let _ = vardiff_task.await;
    }
}

struct SessionState {
    authorized: bool,
    bootstrap_complete: bool,
    bootstrap_inflight: bool,
    worker: String,
    user_agent: Option<String>,
    extranonce1: String,
    extranonce1_bytes: Vec<u8>,
    difficulty: f64,
    share_target_cache: [u8; 32],
    vardiff: VardiffController,
    vardiff_enabled: bool,
    jobs: VecDeque<Arc<SessionJob>>,
    next_job_id: u32,
    last_notify: chrono::DateTime<chrono::Utc>,
    last_prevhash: Option<String>,
    session_start: chrono::DateTime<chrono::Utc>,
    last_clean_jobs_time: Option<chrono::DateTime<chrono::Utc>>,
    notify_bucket: NotifyBucket,
    version_mask: u32,
    submitted_hashes: HashSet<DupKey>,
    submitted_hashes_order: VecDeque<DupKey>,
}

impl SessionState {
    fn new(
        extranonce1: String,
        extranonce1_bytes: Vec<u8>,
        initial_diff: f64,
        target_share_time: f64,
        retarget_time: f64,
        min_diff: f64,
        max_diff: f64,
        vardiff_enabled: bool,
        bucket_capacity: f64,
        bucket_refill_ms: f64,
    ) -> Self {
        Self {
            authorized: false,
            bootstrap_complete: false,
            bootstrap_inflight: false,
            worker: String::new(),
            user_agent: None,
            extranonce1,
            extranonce1_bytes,
            difficulty: initial_diff,
            share_target_cache: share_target_le(initial_diff).unwrap_or([0xff; 32]),
            vardiff: VardiffController::new(target_share_time, retarget_time, min_diff, max_diff),
            vardiff_enabled,
            jobs: VecDeque::with_capacity(16),
            next_job_id: 1,
            last_notify: Utc::now(),
            last_prevhash: None,
            session_start: Utc::now(),
            last_clean_jobs_time: None,
            notify_bucket: NotifyBucket::new(bucket_capacity, bucket_refill_ms),
            version_mask: 0x1fffe000,
            submitted_hashes: HashSet::with_capacity(256),
            submitted_hashes_order: VecDeque::with_capacity(256),
        }
    }

    fn begin_authorized_session(&mut self) {
        self.authorized = true;
        self.bootstrap_complete = false;
        self.bootstrap_inflight = false;
        self.jobs.clear();
        self.last_prevhash = None;
        self.last_clean_jobs_time = None;
        self.submitted_hashes.clear();
        self.submitted_hashes_order.clear();
    }

    fn claim_bootstrap_notify(&mut self) -> bool {
        if self.bootstrap_complete || self.bootstrap_inflight {
            return false;
        }
        self.bootstrap_inflight = true;
        true
    }

    fn mark_jobs_stale_block(&mut self) {
        for job in &self.jobs {
            job.is_stale_block.store(true, Ordering::Release);
        }
        self.jobs.retain(|job| !job.is_stale_block.load(Ordering::Relaxed));
    }

    fn should_clean_jobs(&self, job: &JobTemplate) -> bool {
        !job.submit_old
            || self
                .last_prevhash
                .as_deref()
                .map_or(true, |prev| prev != job.prevhash_le)
    }

    fn note_notify_sent(&mut self, post_send: NotifyPostSend) {
        let now = Utc::now();
        self.last_notify = now;
        self.last_prevhash = Some(post_send.prevhash);
        if post_send.clean_jobs {
            self.last_clean_jobs_time = Some(now);
        }
        if post_send.complete_bootstrap {
            self.bootstrap_complete = true;
            self.bootstrap_inflight = false;
        }
    }

    fn set_difficulty(&mut self, difficulty: f64) {
        match share_target_le(difficulty) {
            Ok(target) => {
                self.difficulty = difficulty;
                self.share_target_cache = target;
            }
            Err(err) => {
                warn!("failed to derive share target for diff {difficulty}: {err:?}");
            }
        }
    }

    fn push_job(
        &mut self,
        job: Arc<JobTemplate>,
        difficulty: f64,
        extranonce1_bytes: &[u8],
        notify_ntime: Option<String>,
    ) -> Arc<SessionJob> {
        if self.next_job_id == 0 {
            self.next_job_id = 1;
        }
        let session_job_id = format_session_job_id(self.next_job_id);
        self.next_job_id = self.next_job_id.wrapping_add(1).max(1);
        self.set_difficulty(difficulty);
        let share_target = self.share_target_cache;

        let mut coinbase_prefix =
            Vec::with_capacity(job.coinbase1_bytes.len() + extranonce1_bytes.len());
        coinbase_prefix.extend_from_slice(&job.coinbase1_bytes);
        coinbase_prefix.extend_from_slice(extranonce1_bytes);

        let (custom_coinbase2_hex, custom_coinbase2_bytes) = (None, None);

        let notify_ntime = notify_ntime.unwrap_or_else(|| job.ntime.clone());
        let entry = Arc::new(SessionJob {
            job,
            difficulty,
            share_target_le: share_target,
            coinbase_prefix: Arc::new(coinbase_prefix),
            session_job_id,
            notify_ntime,
            custom_coinbase2_hex,
            custom_coinbase2_bytes,
            is_stale_block: AtomicBool::new(false),
        });

        self.jobs.push_back(entry.clone());
        while self.jobs.len() > 32 {
            self.jobs.pop_front();
        }
        entry
    }

    fn find_job(&self, job_id: &str) -> Option<Arc<SessionJob>> {
        let job_id = normalize_session_job_id(job_id)?;
        self.jobs
            .iter()
            .rev()
            .find(|entry| {
                entry.session_job_id == job_id
                    && !entry.is_stale_block.load(Ordering::Acquire)
            })
            .cloned()
    }
}

struct SessionJob {
    job: Arc<JobTemplate>,
    difficulty: f64,
    share_target_le: [u8; 32],
    coinbase_prefix: Arc<Vec<u8>>,
    session_job_id: String,
    notify_ntime: String,
    custom_coinbase2_hex: Option<String>,
    custom_coinbase2_bytes: Option<Arc<Vec<u8>>>,
    is_stale_block: AtomicBool,
}

struct NotifyDispatch {
    message: String,
    trace: NotifyTrace,
}

struct NotifyPostSend {
    prevhash: String,
    clean_jobs: bool,
    complete_bootstrap: bool,
}

impl NotifyPostSend {
    fn new(prevhash: String, clean_jobs: bool, complete_bootstrap: bool) -> Self {
        Self {
            prevhash,
            clean_jobs,
            complete_bootstrap,
        }
    }
}

struct NotifyTrace {
    trigger: &'static str,
    template_source: TemplateSource,
    miner: String,
    difficulty: f64,
    session_job_id: String,
    template_job_id: String,
    height: u64,
    clean_jobs: bool,
    notify_ntime: String,
    template_created_at: chrono::DateTime<chrono::Utc>,
}

impl NotifyTrace {
    fn log_sent(&self) {
        let sent_at = Utc::now();
        let template_age_ms = (sent_at - self.template_created_at).num_milliseconds().max(0);
        info!(
            "mining.notify sent trigger={} template_source={} miner={} session_job_id={} template_job_id={} height={} clean_jobs={} diff={:.0} ntime={} job_time={} template_at={} sent_at={} template_age_ms={}",
            self.trigger,
            self.template_source,
            self.miner,
            self.session_job_id,
            self.template_job_id,
            self.height,
            self.clean_jobs,
            self.difficulty,
            self.notify_ntime,
            format_notify_ntime(&self.notify_ntime),
            self.template_created_at.to_rfc3339(),
            sent_at.to_rfc3339(),
            template_age_ms,
        );
    }
}

fn build_notify_dispatch(
    miner: &str,
    difficulty: f64,
    session_job: &SessionJob,
    clean_jobs: bool,
    trigger: &'static str,
) -> NotifyDispatch {
    NotifyDispatch {
        message: build_notify_with_ntime(
            &session_job.session_job_id,
            session_job.job.as_ref(),
            &session_job.notify_ntime,
            clean_jobs,
            session_job.custom_coinbase2_hex.as_deref(),
        ),
        trace: NotifyTrace {
            trigger,
            template_source: session_job.job.template_source,
            miner: miner.to_string(),
            difficulty,
            session_job_id: session_job.session_job_id.clone(),
            template_job_id: session_job.job.job_id.clone(),
            height: session_job.job.height,
            clean_jobs,
            notify_ntime: session_job.notify_ntime.clone(),
            template_created_at: session_job.job.created_at.clone(),
        },
    }
}

async fn send_notify(
    tx: &mpsc::Sender<String>,
    dispatch: NotifyDispatch,
) -> Result<NotifyTrace, tokio::sync::mpsc::error::SendError<String>> {
    let NotifyDispatch { message, trace } = dispatch;
    tx.send(message).await?;
    trace.log_sent();
    Ok(trace)
}

fn normalize_user_agent(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(96).collect())
    }
}

fn build_miner_label(user_agent: Option<&str>, session_id: &str) -> String {
    let base = user_agent.unwrap_or("Unknown");
    let short_id = session_id.chars().take(8).collect::<String>();
    format!("{base} #{short_id}")
}

fn format_session_job_id(job_id: u32) -> String {
    let job_id = format!("{job_id:08x}");
    debug_assert_eq!(job_id.len(), SESSION_JOB_ID_HEX_LEN);
    job_id
}

fn parse_session_job_id(job_id: &str) -> Option<u32> {
    if job_id.len() != SESSION_JOB_ID_HEX_LEN
        || !job_id.as_bytes().iter().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }

    u32::from_str_radix(job_id, 16).ok()
}

fn normalize_session_job_id(job_id: &str) -> Option<String> {
    parse_session_job_id(job_id).map(format_session_job_id)
}

fn format_notify_ntime(ntime_hex: &str) -> String {
    u32::from_str_radix(ntime_hex.trim_start_matches("0x"), 16)
        .ok()
        .and_then(|ts| Utc.timestamp_opt(ts as i64, 0).single())
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "invalid".to_string())
}

fn build_notify_with_ntime(
    job_id: &str,
    job: &JobTemplate,
    ntime: &str,
    clean_jobs: bool,
    custom_coinbase2: Option<&str>,
) -> String {
    let coinbase2 = custom_coinbase2.unwrap_or(&job.coinbase2);
    let cap = 80
        + job_id.len()
        + job.prevhash.len()
        + job.coinbase1.len()
        + coinbase2.len()
        + job.version.len()
        + job.nbits.len()
        + ntime.len()
        + job
            .merkle_branches
            .iter()
            .map(|branch| branch.len() + 3)
            .sum::<usize>();
    let mut out = String::with_capacity(cap);
    out.push_str(r#"{"id":null,"method":"mining.notify","params":[""#);
    out.push_str(job_id);
    out.push_str(r#"",""#);
    out.push_str(&job.prevhash);
    out.push_str(r#"",""#);
    out.push_str(&job.coinbase1);
    out.push_str(r#"",""#);
    out.push_str(coinbase2);
    out.push_str("\",[");
    for (idx, branch) in job.merkle_branches.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(branch);
        out.push('"');
    }
    out.push_str("],\"");
    out.push_str(&job.version);
    out.push_str("\",\"");
    out.push_str(&job.nbits);
    out.push_str("\",\"");
    out.push_str(ntime);
    out.push('"');
    out.push(',');
    out.push_str(if clean_jobs { "true]}" } else { "false]}" });
    out
}

fn build_set_difficulty(diff: f64) -> String {
    json!({
        "id": null,
        "method": "mining.set_difficulty",
        "params": [diff]
    })
    .to_string()
}

fn parse_u32_be(hex_str: &str) -> Option<u32> {
    u32::from_str_radix(hex_str, 16).ok()
}

fn build_dup_key(
    job_id: &str,
    nonce_hex: &str,
    ntime_hex: &str,
    extranonce2_hex: &str,
    version_hex: &str,
) -> DupKey {
    let mut extranonce2 = [0u8; 16];
    let decoded = hex::decode(extranonce2_hex).unwrap_or_default();
    let extra_len = decoded.len().min(extranonce2.len());
    extranonce2[..extra_len].copy_from_slice(&decoded[..extra_len]);

    (
        parse_session_job_id(job_id).unwrap_or(0),
        u32::from_str_radix(nonce_hex, 16).unwrap_or(0),
        u32::from_str_radix(ntime_hex, 16).unwrap_or(0),
        extranonce2,
        extra_len as u8,
        u32::from_str_radix(version_hex, 16).unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_shutdown_terminates_background_tasks_even_with_sender_clones() {
        let (tx, mut rx) = mpsc::channel::<String>(1);
        let writer_task = tokio::spawn(async move {
            while rx.recv().await.is_some() {}
        });
        let notify_tx = tx.clone();
        let notify_task = tokio::spawn(async move {
            let _held_sender = notify_tx;
            std::future::pending::<()>().await;
        });
        let vardiff_tx = tx.clone();
        let vardiff_task = tokio::spawn(async move {
            let _held_sender = vardiff_tx;
            std::future::pending::<()>().await;
        });

        let tasks = SessionTasks::new(tx, writer_task, notify_task, vardiff_task);

        tokio::time::timeout(Duration::from_secs(1), tasks.shutdown())
            .await
            .expect("session shutdown should not hang");
    }

    #[test]
    fn push_job_recomputes_share_target_when_difficulty_was_preupdated() {
        let mut state = SessionState::new(
            "en1".to_string(),
            vec![1, 2, 3, 4],
            1.0,
            15.0,
            90.0,
            1.0,
            1024.0,
            true,
            2.0,
            500.0,
        );
        state.difficulty = 4.0;

        let session_job = state.push_job(Arc::new(JobTemplate::empty()), 4.0, &[], None);

        assert_eq!(session_job.share_target_le, share_target_le(4.0).unwrap());
    }

    #[test]
    fn duplicate_key_keeps_high_extranonce2_bytes_distinct() {
        let a = build_dup_key(
            "00000001",
            "00000001",
            "00000002",
            "010000000000000000",
            "20000000",
        );
        let b = build_dup_key(
            "00000001",
            "00000001",
            "00000002",
            "020000000000000000",
            "20000000",
        );

        assert_ne!(a, b);
    }

    #[test]
    fn session_job_ids_are_fixed_8_hex_and_wrap_without_zero() {
        let mut state = SessionState::new(
            "en1".to_string(),
            vec![1, 2, 3, 4],
            1.0,
            15.0,
            90.0,
            1.0,
            1024.0,
            true,
            2.0,
            500.0,
        );
        let job = Arc::new(JobTemplate::empty());

        state.next_job_id = u32::MAX - 1;
        let penultimate = state.push_job(job.clone(), 1.0, &[], None);
        let last = state.push_job(job.clone(), 1.0, &[], None);
        let wrapped = state.push_job(job, 1.0, &[], None);

        assert_eq!(penultimate.session_job_id, "fffffffe");
        assert_eq!(last.session_job_id, "ffffffff");
        assert_eq!(wrapped.session_job_id, "00000001");
    }

    #[test]
    fn normalize_session_job_id_requires_exactly_8_hex_chars() {
        assert_eq!(
            normalize_session_job_id("AbCd1234").as_deref(),
            Some("abcd1234")
        );
        assert!(normalize_session_job_id("1234567").is_none());
        assert!(normalize_session_job_id("123456789").is_none());
        assert!(normalize_session_job_id("0x12345678").is_none());
        assert!(normalize_session_job_id("zzzz1234").is_none());
    }

    #[test]
    fn clean_jobs_prunes_stale_entries_from_find_job() {
        let mut state = SessionState::new(
            "en1".to_string(),
            vec![1, 2, 3, 4],
            1.0,
            15.0,
            90.0,
            1.0,
            1024.0,
            true,
            2.0,
            500.0,
        );

        let job = Arc::new(JobTemplate::empty());
        let session_job = state.push_job(job, 1.0, &[], None);

        assert!(state.find_job(&session_job.session_job_id).is_some());

        state.mark_jobs_stale_block();

        assert!(state.find_job(&session_job.session_job_id).is_none());
        assert!(state.jobs.is_empty());
    }

    #[test]
    fn submitold_false_forces_clean_jobs_even_when_prevhash_is_unchanged() {
        let mut state = SessionState::new(
            "en1".to_string(),
            vec![1, 2, 3, 4],
            1.0,
            15.0,
            90.0,
            1.0,
            1024.0,
            true,
            2.0,
            500.0,
        );
        state.last_prevhash = Some("same-prevhash".to_string());

        let mut job = JobTemplate::empty();
        job.ready = true;
        job.prevhash_le = "same-prevhash".to_string();
        job.submit_old = false;

        assert!(state.should_clean_jobs(&job));

        job.submit_old = true;
        assert!(!state.should_clean_jobs(&job));
    }

    #[test]
    fn bootstrap_claim_is_single_shot_until_notify_send_commits() {
        let mut state = SessionState::new(
            "en1".to_string(),
            vec![1, 2, 3, 4],
            1.0,
            15.0,
            90.0,
            1.0,
            1024.0,
            true,
            2.0,
            500.0,
        );

        state.begin_authorized_session();

        assert!(state.claim_bootstrap_notify());
        assert!(!state.claim_bootstrap_notify());
        assert!(!state.bootstrap_complete);
        assert!(state.bootstrap_inflight);

        state.note_notify_sent(NotifyPostSend::new(
            "prevhash-1".to_string(),
            true,
            true,
        ));

        assert!(state.bootstrap_complete);
        assert!(!state.bootstrap_inflight);
        assert_eq!(state.last_prevhash.as_deref(), Some("prevhash-1"));
        assert!(state.last_clean_jobs_time.is_some());
    }

    #[test]
    fn begin_authorized_session_clears_prior_job_state_and_resets_bootstrap_gate() {
        let mut state = SessionState::new(
            "en1".to_string(),
            vec![1, 2, 3, 4],
            1.0,
            15.0,
            90.0,
            1.0,
            1024.0,
            true,
            2.0,
            500.0,
        );
        state.authorized = true;
        state.bootstrap_complete = true;
        state.bootstrap_inflight = true;
        state.last_prevhash = Some("old-prevhash".to_string());
        state.last_clean_jobs_time = Some(Utc::now());
        state.jobs.push_back(Arc::new(SessionJob {
            job: Arc::new(JobTemplate::empty()),
            difficulty: 1.0,
            share_target_le: share_target_le(1.0).unwrap(),
            coinbase_prefix: Arc::new(Vec::new()),
            session_job_id: "00000001".to_string(),
            notify_ntime: "00000000".to_string(),
            custom_coinbase2_hex: None,
            custom_coinbase2_bytes: None,
            is_stale_block: AtomicBool::new(false),
        }));
        state.submitted_hashes.insert((
            1,
            2,
            3,
            [4; 16],
            5,
            6,
        ));
        state.submitted_hashes_order.push_back((
            1,
            2,
            3,
            [4; 16],
            5,
            6,
        ));

        state.begin_authorized_session();

        assert!(state.authorized);
        assert!(!state.bootstrap_complete);
        assert!(!state.bootstrap_inflight);
        assert!(state.jobs.is_empty());
        assert!(state.submitted_hashes.is_empty());
        assert!(state.submitted_hashes_order.is_empty());
        assert!(state.last_prevhash.is_none());
        assert!(state.last_clean_jobs_time.is_none());
    }
}
