use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use binary_sv2::{Seq0255, Str0255, Sv2Option, B032, B064K, U256};
use chrono::Utc;
use common_messages_sv2::{
    Protocol, SetupConnection, SetupConnectionError, SetupConnectionSuccess,
    CHANNEL_BIT_SETUP_CONNECTION_ERROR, CHANNEL_BIT_SETUP_CONNECTION_SUCCESS,
    MESSAGE_TYPE_SETUP_CONNECTION, MESSAGE_TYPE_SETUP_CONNECTION_ERROR,
    MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
};
use mining_sv2::{
    NewExtendedMiningJob, OpenExtendedMiningChannel, OpenExtendedMiningChannelSuccess,
    OpenMiningChannelError, SetNewPrevHash, SetTarget, SubmitSharesError, SubmitSharesExtended,
    SubmitSharesSuccess, UpdateChannelError, CHANNEL_BIT_MINING_SET_NEW_PREV_HASH,
    CHANNEL_BIT_NEW_EXTENDED_MINING_JOB, CHANNEL_BIT_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
    CHANNEL_BIT_OPEN_MINING_CHANNEL_ERROR, CHANNEL_BIT_SET_TARGET, CHANNEL_BIT_SUBMIT_SHARES_ERROR,
    CHANNEL_BIT_SUBMIT_SHARES_SUCCESS, CHANNEL_BIT_UPDATE_CHANNEL_ERROR,
    MESSAGE_TYPE_CLOSE_CHANNEL, MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
    MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
    MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS, MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR,
    MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL, MESSAGE_TYPE_SET_TARGET,
    MESSAGE_TYPE_SUBMIT_SHARES_ERROR, MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
    MESSAGE_TYPE_SUBMIT_SHARES_STANDARD, MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
    MESSAGE_TYPE_UPDATE_CHANNEL, MESSAGE_TYPE_UPDATE_CHANNEL_ERROR,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::block_archive::maybe_archive_candidate_block;
use crate::config::Config;
use crate::metrics::{BlockEvent, MetricsStore};
use crate::share::{share_target_le, validate_share, ShareSubmit};
use crate::template::{JobTemplate, SubmitBlockOutcome, TemplateEngine};
use crate::vardiff::VardiffController;

use super::transport::{random_bytes, Sv2RawFrame, Sv2Reader, Sv2Transport, Sv2Writer};

const SV2_VERSION: u16 = 2;
const MAX_RECENT_SHARES: usize = 4096;
const HASHES_PER_DIFF: f64 = 4_294_967_296.0;
const FLAG_REQUIRES_VERSION_ROLLING: u32 = 0b100;
const SUPPORTED_MINING_FLAGS: u32 = FLAG_REQUIRES_VERSION_ROLLING;
const ALLOWED_VERSION_ROLLING_MASK: u32 = 0x1fffe000;

type DupKey = (u32, u32, u32, u32, [u8; 32], u8, u32);

pub struct Sv2Connection {
    config: Config,
    template_engine: Arc<TemplateEngine>,
    metrics: MetricsStore,
    reader: Option<Sv2Reader>,
    writer: Sv2Writer,
    peer_addr: SocketAddr,
    state: ConnectionState,
}

#[derive(Debug)]
struct ConnectionState {
    setup_complete: bool,
    negotiated_flags: u32,
    downstream_user_agent: Option<String>,
    next_channel_id: u32,
    next_job_id: u32,
    channels: HashMap<u32, ChannelState>,
    submitted: HashSet<DupKey>,
    submitted_order: VecDeque<DupKey>,
}

impl ConnectionState {
    fn new() -> Self {
        Self {
            setup_complete: false,
            negotiated_flags: 0,
            downstream_user_agent: None,
            next_channel_id: 1,
            next_job_id: 1,
            channels: HashMap::new(),
            submitted: HashSet::with_capacity(256),
            submitted_order: VecDeque::with_capacity(256),
        }
    }

    fn allocate_channel_id(&mut self) -> u32 {
        let id = self.next_channel_id.max(1);
        self.next_channel_id = self.next_channel_id.wrapping_add(1).max(1);
        id
    }

    fn allocate_job_id(&mut self) -> u32 {
        let id = self.next_job_id.max(1);
        self.next_job_id = self.next_job_id.wrapping_add(1).max(1);
        id
    }

    fn remember_share(&mut self, key: DupKey) -> bool {
        if self.submitted.contains(&key) {
            return false;
        }
        if self.submitted.len() >= MAX_RECENT_SHARES {
            if let Some(oldest) = self.submitted_order.pop_front() {
                self.submitted.remove(&oldest);
            }
        }
        self.submitted.insert(key);
        self.submitted_order.push_back(key);
        true
    }
}

#[derive(Debug, Clone)]
struct ChannelState {
    worker: String,
    extranonce_prefix: Vec<u8>,
    extranonce_size: usize,
    difficulty: f64,
    share_target_le: [u8; 32],
    target_sent: bool,
    vardiff: VardiffController,
    jobs: HashMap<u32, ChannelJob>,
    current_job_id: Option<u32>,
}

#[derive(Debug, Clone)]
struct ChannelJob {
    template: Arc<JobTemplate>,
    coinbase_prefix: Arc<Vec<u8>>,
    share_target_le: [u8; 32],
    difficulty: f64,
    created_at: chrono::DateTime<Utc>,
    version_rolling_allowed: bool,
    version_mask: u32,
}

impl Sv2Connection {
    pub fn new(
        config: Config,
        template_engine: Arc<TemplateEngine>,
        metrics: MetricsStore,
        transport: Sv2Transport,
        peer_addr: SocketAddr,
    ) -> Self {
        let (reader, writer) = transport.split();
        Self {
            config,
            template_engine,
            metrics,
            reader: Some(reader),
            writer,
            peer_addr,
            state: ConnectionState::new(),
        }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let setup = self
            .reader
            .as_mut()
            .ok_or_else(|| anyhow!("sv2 reader missing before setup"))?
            .read_frame()
            .await?;
        self.handle_setup_connection(setup).await?;
        let mut frame_rx = self.spawn_reader();
        let mut template_rx = self.template_engine.subscribe();
        let interval_ms = (self.config.vardiff_retarget_time_secs * 1000.0).max(1000.0) as u64;
        let mut vardiff_interval = tokio::time::interval(Duration::from_millis(interval_ms));
        vardiff_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            match self
                .next_event(&mut template_rx, &mut frame_rx, &mut vardiff_interval)
                .await?
            {
                ConnectionEvent::Frame(frame) => self.handle_frame(frame).await?,
                ConnectionEvent::Template(job) if job.ready => {
                    self.send_job_to_all_channels(job, false).await?;
                }
                ConnectionEvent::Template(_) => {}
                ConnectionEvent::VardiffTick => {
                    self.retarget_all_channels(Utc::now()).await?;
                }
            }
        }
    }

    fn spawn_reader(&mut self) -> mpsc::UnboundedReceiver<anyhow::Result<Sv2RawFrame>> {
        let mut reader = self.reader.take().expect("sv2 reader already spawned");
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let result = reader.read_frame().await;
                let done = result.is_err();
                if tx.send(result).is_err() || done {
                    break;
                }
            }
        });
        rx
    }

    async fn next_event(
        &mut self,
        template_rx: &mut tokio::sync::watch::Receiver<Arc<JobTemplate>>,
        frame_rx: &mut mpsc::UnboundedReceiver<anyhow::Result<Sv2RawFrame>>,
        vardiff_interval: &mut tokio::time::Interval,
    ) -> anyhow::Result<ConnectionEvent> {
        tokio::select! {
            frame = frame_rx.recv() => {
                let frame = frame.ok_or_else(|| anyhow!("sv2 reader closed"))??;
                Ok(ConnectionEvent::Frame(frame))
            }
            changed = template_rx.changed() => {
                changed.context("sv2 template watch closed")?;
                Ok(ConnectionEvent::Template(template_rx.borrow().clone()))
            }
            _ = vardiff_interval.tick(), if self.config.vardiff_enabled => {
                Ok(ConnectionEvent::VardiffTick)
            }
        }
    }

    async fn handle_setup_connection(&mut self, frame: Sv2RawFrame) -> anyhow::Result<()> {
        if frame.msg_type != MESSAGE_TYPE_SETUP_CONNECTION || frame.channel_msg {
            self.send_setup_error(0, "unsupported-protocol").await?;
            bail!("expected SetupConnection as first sv2 message");
        }

        let setup = decode_setup_connection(&frame.payload).context("decode SetupConnection")?;
        match decide_setup(&setup) {
            SetupDecision::Accept { flags } => {
                let downstream_user_agent = setup_connection_user_agent(&setup);
                self.writer
                    .send_message(
                        SetupConnectionSuccess {
                            used_version: SV2_VERSION,
                            flags,
                        },
                        MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
                        CHANNEL_BIT_SETUP_CONNECTION_SUCCESS,
                    )
                    .await?;
                self.state.setup_complete = true;
                self.state.negotiated_flags = flags;
                self.state.downstream_user_agent = downstream_user_agent.clone();
                self.metrics.counters.inc_sv2_setup_accepted();
                info!(
                    peer = %self.peer_addr,
                    vendor = %setup.vendor.as_utf8_or_hex(),
                    firmware = %setup.firmware.as_utf8_or_hex(),
                    "sv2 setup accepted"
                );
                Ok(())
            }
            SetupDecision::Reject { flags, code } => {
                self.metrics.counters.inc_sv2_setup_rejected();
                self.send_setup_error(flags, code).await?;
                bail!("sv2 setup rejected: {code}");
            }
        }
    }

    async fn send_setup_error(&mut self, flags: u32, code: &str) -> anyhow::Result<()> {
        self.writer
            .send_message(
                SetupConnectionError {
                    flags,
                    error_code: str0255(code)?,
                },
                MESSAGE_TYPE_SETUP_CONNECTION_ERROR,
                CHANNEL_BIT_SETUP_CONNECTION_ERROR,
            )
            .await
    }

    async fn handle_frame(&mut self, frame: Sv2RawFrame) -> anyhow::Result<()> {
        if frame.extension_type != 0 {
            self.metrics.counters.inc_sv2_unsupported_message();
            bail!("unsupported sv2 extension type {}", frame.extension_type);
        }

        match frame.msg_type {
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL => {
                let request = decode_open_extended_channel(&frame.payload)
                    .context("decode OpenExtendedMiningChannel")?;
                self.handle_open_extended_channel(request).await
            }
            MESSAGE_TYPE_OPEN_STANDARD_MINING_CHANNEL => {
                let request_id = first_u32_le(&frame.payload).unwrap_or_default();
                self.metrics.counters.inc_sv2_channel_open_error();
                self.metrics.counters.inc_sv2_unsupported_message();
                self.writer
                    .send_message(
                        OpenMiningChannelError {
                            request_id,
                            error_code: str0255("unsupported-channel-type")?,
                        },
                        MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR,
                        CHANNEL_BIT_OPEN_MINING_CHANNEL_ERROR,
                    )
                    .await
            }
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED => {
                let submit = decode_submit_extended(&frame.payload)
                    .context("decode SubmitSharesExtended")?;
                self.handle_submit_extended(submit).await
            }
            MESSAGE_TYPE_SUBMIT_SHARES_STANDARD => {
                let (channel_id, sequence_number) = first_two_u32_le(&frame.payload);
                self.metrics.counters.inc_sv2_submit_rejected();
                self.metrics.counters.inc_sv2_unsupported_message();
                self.send_submit_error(
                    channel_id.unwrap_or_default(),
                    sequence_number.unwrap_or_default(),
                    "unsupported-channel-type",
                )
                .await
            }
            MESSAGE_TYPE_UPDATE_CHANNEL => {
                let (channel_id, _) = first_two_u32_le(&frame.payload);
                self.metrics.counters.inc_sv2_unsupported_message();
                self.send_update_channel_error(
                    channel_id.unwrap_or_default(),
                    "max-target-out-of-range",
                )
                .await
            }
            MESSAGE_TYPE_CLOSE_CHANNEL => {
                let channel_id = first_u32_le(&frame.payload).unwrap_or_default();
                if self.state.channels.remove(&channel_id).is_some() {
                    self.metrics.counters.inc_sv2_channel_closed();
                }
                info!(
                    peer = %self.peer_addr,
                    channel_id,
                    "sv2 channel closed by downstream"
                );
                Ok(())
            }
            other => {
                self.metrics.counters.inc_sv2_unsupported_message();
                bail!("unsupported sv2 mining message type 0x{other:02x}")
            }
        }
    }

    async fn handle_open_extended_channel(
        &mut self,
        request: OpenExtendedMiningChannel<'static>,
    ) -> anyhow::Result<()> {
        let min_extranonce_size = usize::from(request.min_extranonce_size);
        if min_extranonce_size > 32 {
            self.metrics.counters.inc_sv2_channel_open_error();
            self.writer
                .send_message(
                    OpenMiningChannelError::unsupported_extranonce_size(request.request_id),
                    MESSAGE_TYPE_OPEN_MINING_CHANNEL_ERROR,
                    CHANNEL_BIT_OPEN_MINING_CHANNEL_ERROR,
                )
                .await?;
            return Ok(());
        }

        let channel_id = self.state.allocate_channel_id();
        let extranonce_prefix_len = self.config.extranonce1_size.clamp(1, 32);
        let extranonce_size = self
            .config
            .extranonce2_size
            .max(min_extranonce_size)
            .min(32);
        let extranonce_prefix = random_bytes(extranonce_prefix_len);
        let difficulty = sv2_initial_difficulty(&self.config, request.nominal_hash_rate);
        let share_target = share_target_le(difficulty)?;
        let user_identity = request.user_identity.as_utf8_or_hex();
        let session_id = sv2_session_id(&extranonce_prefix);
        let worker = sv2_worker_label(self.state.downstream_user_agent.as_deref(), &session_id);

        let channel = ChannelState {
            worker: worker.clone(),
            extranonce_prefix: extranonce_prefix.clone(),
            extranonce_size,
            difficulty,
            share_target_le: share_target,
            target_sent: false,
            vardiff: VardiffController::new(
                self.config.target_share_time_secs,
                self.config.vardiff_retarget_time_secs,
                self.config.min_difficulty,
                self.config.max_difficulty,
            ),
            jobs: HashMap::new(),
            current_job_id: None,
        };
        self.state.channels.insert(channel_id, channel);

        self.writer
            .send_message(
                OpenExtendedMiningChannelSuccess {
                    request_id: request.request_id,
                    channel_id,
                    target: u256_from_le(share_target),
                    extranonce_size: extranonce_size as u16,
                    extranonce_prefix: b032(extranonce_prefix)?,
                    group_channel_id: 0,
                },
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
                CHANNEL_BIT_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
            )
            .await?;

        self.metrics
            .record_miner_seen(
                &worker,
                difficulty,
                self.state.downstream_user_agent.clone(),
                Some(session_id.clone()),
                Some("SV2".to_string()),
            )
            .await;
        self.metrics.counters.inc_sv2_channel_opened();

        let current = self.template_engine.current_job().await;
        if current.ready {
            self.send_job_to_channel(channel_id, current, true).await?;
        }

        info!(
            peer = %self.peer_addr,
            channel_id,
            worker = %user_identity,
            session_id = %session_id,
            extranonce_size,
            "sv2 extended channel opened"
        );
        Ok(())
    }

    async fn send_job_to_channel(
        &mut self,
        channel_id: u32,
        job: Arc<JobTemplate>,
        clean: bool,
    ) -> anyhow::Result<()> {
        let job_id = self.state.allocate_job_id();
        let version_mask = version_mask_for_job(job.as_ref());
        let version_rolling_allowed = self.version_rolling_negotiated() && version_mask != 0;
        let channel = self
            .state
            .channels
            .get_mut(&channel_id)
            .ok_or_else(|| anyhow!("invalid sv2 channel {channel_id}"))?;

        let mut validation_coinbase_prefix =
            Vec::with_capacity(job.coinbase1_bytes.len() + channel.extranonce_prefix.len());
        validation_coinbase_prefix.extend_from_slice(&job.coinbase1_bytes);
        validation_coinbase_prefix.extend_from_slice(&channel.extranonce_prefix);

        let channel_job = ChannelJob {
            template: job.clone(),
            coinbase_prefix: Arc::new(validation_coinbase_prefix),
            share_target_le: channel.share_target_le,
            difficulty: channel.difficulty,
            created_at: Utc::now(),
            version_rolling_allowed,
            version_mask,
        };
        channel.jobs.insert(job_id, channel_job);
        channel.current_job_id = Some(job_id);
        if clean {
            prune_old_jobs(channel, job_id);
        }

        let send_target = !channel.target_sent;
        let share_target_le = channel.share_target_le;
        if send_target {
            channel.target_sent = true;
        }

        if send_target {
            self.writer
                .send_message(
                    SetTarget {
                        channel_id,
                        maximum_target: u256_from_le(share_target_le),
                    },
                    MESSAGE_TYPE_SET_TARGET,
                    CHANNEL_BIT_SET_TARGET,
                )
                .await?;
        }

        let sv2_min_ntime = sv2_min_ntime(job.as_ref());
        self.writer
            .send_message(
                NewExtendedMiningJob {
                    channel_id,
                    job_id,
                    min_ntime: new_extended_job_min_ntime(clean, job.as_ref()),
                    version: job.version_u32,
                    version_rolling_allowed,
                    merkle_path: merkle_path(&job.merkle_branches_le)?,
                    coinbase_tx_prefix: b064k(job.coinbase1_bytes.clone())?,
                    coinbase_tx_suffix: b064k(job.coinbase2_bytes.clone())?,
                },
                MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
                CHANNEL_BIT_NEW_EXTENDED_MINING_JOB,
            )
            .await?;

        if clean {
            self.writer
                .send_message(
                    SetNewPrevHash {
                        channel_id,
                        job_id,
                        prev_hash: u256_from_le(job.prevhash_le_bytes),
                        min_ntime: sv2_min_ntime,
                        nbits: job.nbits_u32,
                    },
                    MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
                    CHANNEL_BIT_MINING_SET_NEW_PREV_HASH,
                )
                .await?;
        }

        self.metrics.counters.inc_jobs_sent(clean);
        Ok(())
    }

    fn version_rolling_negotiated(&self) -> bool {
        self.state.negotiated_flags & FLAG_REQUIRES_VERSION_ROLLING != 0
    }

    async fn send_job_to_all_channels(
        &mut self,
        job: Arc<JobTemplate>,
        force_clean: bool,
    ) -> anyhow::Result<()> {
        let channel_ids = self.state.channels.keys().copied().collect::<Vec<_>>();
        for channel_id in channel_ids {
            if let Some(channel) = self.state.channels.get(&channel_id) {
                let clean = force_clean || should_clean_channel_jobs(channel, job.as_ref());
                self.send_job_to_channel(channel_id, job.clone(), clean)
                    .await?;
            }
        }
        Ok(())
    }

    async fn handle_submit_extended(
        &mut self,
        submit: SubmitSharesExtended<'static>,
    ) -> anyhow::Result<()> {
        let submit_start = std::time::Instant::now();
        let channel = match self.state.channels.get(&submit.channel_id) {
            Some(channel) => channel.clone(),
            None => {
                self.metrics.counters.inc_sv2_submit_rejected();
                self.send_submit_error(
                    submit.channel_id,
                    submit.sequence_number,
                    SubmitSharesError::invalid_channel_error_code(),
                )
                .await?;
                return Ok(());
            }
        };

        if submit.extranonce.inner_as_ref().len() != channel.extranonce_size {
            self.metrics.counters.inc_sv2_submit_rejected();
            self.send_submit_error(
                submit.channel_id,
                submit.sequence_number,
                "invalid-extranonce-size",
            )
            .await?;
            return Ok(());
        }

        let channel_job = match channel.jobs.get(&submit.job_id) {
            Some(job) => job.clone(),
            None => {
                self.metrics.counters.inc_sv2_submit_rejected();
                self.send_submit_error(
                    submit.channel_id,
                    submit.sequence_number,
                    SubmitSharesError::invalid_job_id_error_code(),
                )
                .await?;
                return Ok(());
            }
        };

        let version = match resolve_submitted_version(&channel_job, submit.version) {
            Ok(version) => version,
            Err(_) => {
                self.metrics.counters.inc_version_rolling_violation();
                self.metrics.counters.inc_sv2_submit_rejected();
                self.send_submit_error(
                    submit.channel_id,
                    submit.sequence_number,
                    SubmitSharesError::difficulty_too_low_error_code(),
                )
                .await?;
                return Ok(());
            }
        };

        let dup_key = build_dup_key(&submit, version);
        if !self.state.remember_share(dup_key) {
            self.metrics.counters.inc_duplicate_share();
            self.metrics.counters.inc_sv2_submit_rejected();
            self.send_submit_error(submit.channel_id, submit.sequence_number, "duplicate-share")
                .await?;
            return Ok(());
        }

        let share_submit = ShareSubmit {
            extranonce2: hex::encode(submit.extranonce.inner_as_ref()),
            ntime: format!("{:08x}", submit.ntime),
            nonce: format!("{:08x}", submit.nonce),
            version: Some(format!("{:08x}", version)),
        };

        let result = validate_share(
            channel_job.template.as_ref(),
            channel_job.coinbase_prefix.as_slice(),
            &share_submit,
            &channel_job.share_target_le,
            None,
        )
        .context("validate sv2 extended share")?;

        let accepted_for_miner = result.accepted || result.is_block;
        if accepted_for_miner {
            self.writer
                .send_message(
                    SubmitSharesSuccess {
                        channel_id: submit.channel_id,
                        last_sequence_number: submit.sequence_number,
                        new_submits_accepted_count: 1,
                        new_shares_sum: channel_job.difficulty.max(1.0).round() as u64,
                    },
                    MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
                    CHANNEL_BIT_SUBMIT_SHARES_SUCCESS,
                )
                .await?;
            self.metrics.counters.inc_sv2_submit_accepted();
        } else {
            self.metrics.counters.inc_sv2_submit_rejected();
            self.send_submit_error(
                submit.channel_id,
                submit.sequence_number,
                SubmitSharesError::difficulty_too_low_error_code(),
            )
            .await?;
        }

        let now = Utc::now();
        let notify_to_submit_ms = (now - channel_job.created_at).num_milliseconds().max(0);
        let job_age_secs = (now - channel_job.created_at).num_seconds().max(0) as u64;
        let notify_delay_ms = notify_to_submit_ms as u64;
        let submit_rtt_ms = submit_start.elapsed().as_micros() as f64 / 1000.0;
        self.metrics
            .record_share(
                &channel.worker,
                channel_job.difficulty,
                result.difficulty,
                accepted_for_miner,
                result.is_block,
                notify_to_submit_ms,
                submit_rtt_ms,
                job_age_secs,
                notify_delay_ms,
                false,
            )
            .await;

        if accepted_for_miner {
            self.maybe_retarget_channel(submit.channel_id, channel_job.difficulty, now)
                .await?;
        }

        if result.is_block {
            self.metrics.counters.inc_sv2_block_found();
            self.submit_block_candidate(&channel.worker, &channel_job, &result)
                .await;
        }

        Ok(())
    }

    async fn maybe_retarget_channel(
        &mut self,
        channel_id: u32,
        share_difficulty: f64,
        now: chrono::DateTime<Utc>,
    ) -> anyhow::Result<()> {
        if !self.config.vardiff_enabled {
            return Ok(());
        }

        let Some((new_difficulty, new_target, worker)) =
            self.retarget_channel_state(channel_id, Some(share_difficulty), now)
        else {
            return Ok(());
        };

        self.send_channel_target(channel_id, new_difficulty, new_target, worker)
            .await
    }

    async fn retarget_all_channels(&mut self, now: chrono::DateTime<Utc>) -> anyhow::Result<()> {
        if !self.config.vardiff_enabled {
            return Ok(());
        }

        let channel_ids = self.state.channels.keys().copied().collect::<Vec<_>>();
        for channel_id in channel_ids {
            let Some((new_difficulty, new_target, worker)) =
                self.retarget_channel_state(channel_id, None, now)
            else {
                continue;
            };
            self.send_channel_target(channel_id, new_difficulty, new_target, worker)
                .await?;
        }
        Ok(())
    }

    async fn send_channel_target(
        &mut self,
        channel_id: u32,
        new_difficulty: f64,
        new_target: [u8; 32],
        worker: String,
    ) -> anyhow::Result<()> {
        self.writer
            .send_message(
                SetTarget {
                    channel_id,
                    maximum_target: u256_from_le(new_target),
                },
                MESSAGE_TYPE_SET_TARGET,
                CHANNEL_BIT_SET_TARGET,
            )
            .await?;
        info!(
            channel_id,
            worker = %worker,
            difficulty = new_difficulty,
            "sv2 vardiff retarget"
        );
        self.metrics
            .record_miner_seen(&worker, new_difficulty, None, None, None)
            .await;
        Ok(())
    }

    fn retarget_channel_state(
        &mut self,
        channel_id: u32,
        share_difficulty: Option<f64>,
        now: chrono::DateTime<Utc>,
    ) -> Option<(f64, [u8; 32], String)> {
        let channel = self.state.channels.get_mut(&channel_id)?;
        if let Some(share_difficulty) = share_difficulty {
            channel.vardiff.record_share(now, share_difficulty);
        }
        let new_difficulty = channel.vardiff.maybe_retarget(channel.difficulty, now)?;
        let new_target = match set_channel_difficulty(channel, new_difficulty) {
            Ok(target) => target,
            Err(err) => {
                warn!(
                    channel_id,
                    difficulty = new_difficulty,
                    "failed to retarget sv2 channel: {err:?}"
                );
                return None;
            }
        };
        Some((new_difficulty, new_target, channel.worker.clone()))
    }

    async fn send_submit_error(
        &mut self,
        channel_id: u32,
        sequence_number: u32,
        code: &str,
    ) -> anyhow::Result<()> {
        self.writer
            .send_message(
                SubmitSharesError {
                    channel_id,
                    sequence_number,
                    error_code: str0255(code)?,
                },
                MESSAGE_TYPE_SUBMIT_SHARES_ERROR,
                CHANNEL_BIT_SUBMIT_SHARES_ERROR,
            )
            .await
    }

    async fn submit_block_candidate(
        &self,
        worker: &str,
        channel_job: &ChannelJob,
        result: &crate::share::ShareResult,
    ) {
        let Some(block_hex) = result.block_hex.clone() else {
            return;
        };
        let coinbase_hex = result.coinbase_hex.clone().unwrap_or_default();
        let job = channel_job.template.clone();
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
        let worker = worker.to_string();

        metrics
            .upsert_block(BlockEvent {
                height,
                hash: block_hash.clone(),
                worker: worker.clone(),
                difficulty,
                status: "candidate".to_string(),
                reason: None,
                archive_path: None,
                created_at: Utc::now(),
            })
            .await;

        info!(
            "*** SV2 BLOCK FOUND worker={} height={} hash={} diff={:.2} ***",
            worker, height, block_hash, difficulty
        );

        tokio::spawn(async move {
            let mut archive_path = None;
            if archive_before_submit {
                archive_path = maybe_archive_candidate_block(
                    archive_dir.clone(),
                    height,
                    &block_hash,
                    &block_hex,
                    &coinbase_hex,
                    &template_key,
                )
                .await;
            }

            let (status, reason) = match engine
                .submit_block(
                    &block_hex,
                    &block_hash,
                    &template_key,
                    &coinbase_hex,
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
                    &block_hex,
                    &coinbase_hex,
                    &template_key,
                )
                .await;
            }

            metrics
                .upsert_block(BlockEvent {
                    height,
                    hash: block_hash,
                    worker,
                    difficulty,
                    status,
                    reason,
                    archive_path,
                    created_at: Utc::now(),
                })
                .await;
        });
    }

    async fn send_update_channel_error(
        &mut self,
        channel_id: u32,
        code: &str,
    ) -> anyhow::Result<()> {
        self.writer
            .send_message(
                UpdateChannelError {
                    channel_id,
                    error_code: str0255(code)?,
                },
                MESSAGE_TYPE_UPDATE_CHANNEL_ERROR,
                CHANNEL_BIT_UPDATE_CHANNEL_ERROR,
            )
            .await
    }
}

enum ConnectionEvent {
    Frame(Sv2RawFrame),
    Template(Arc<JobTemplate>),
    VardiffTick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupDecision {
    Accept { flags: u32 },
    Reject { flags: u32, code: &'static str },
}

fn decide_setup(setup: &SetupConnection<'_>) -> SetupDecision {
    if setup.protocol != Protocol::MiningProtocol {
        return SetupDecision::Reject {
            flags: 0,
            code: "unsupported-protocol",
        };
    }
    if setup.get_version(SV2_VERSION, SV2_VERSION).is_none() {
        return SetupDecision::Reject {
            flags: 0,
            code: "protocol-version-mismatch",
        };
    }
    let unsupported = unsupported_mining_flags(setup.flags);
    if unsupported != 0 {
        return SetupDecision::Reject {
            flags: unsupported,
            code: "unsupported-feature-flags",
        };
    }
    SetupDecision::Accept {
        flags: setup.flags & SUPPORTED_MINING_FLAGS,
    }
}

fn setup_connection_user_agent(setup: &SetupConnection<'_>) -> Option<String> {
    let mut parts = Vec::new();
    for value in [&setup.vendor, &setup.hardware_version, &setup.firmware] {
        if let Some(part) = non_empty_str0255(value) {
            parts.push(part);
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn non_empty_str0255(value: &Str0255<'_>) -> Option<String> {
    let text = value.as_utf8_or_hex().trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn unsupported_mining_flags(flags: u32) -> u32 {
    flags & !SUPPORTED_MINING_FLAGS
}

fn decode_setup_connection(payload: &[u8]) -> anyhow::Result<SetupConnection<'static>> {
    let mut bytes = payload.to_vec();
    binary_sv2::from_bytes::<SetupConnection<'_>>(&mut bytes)
        .map(|msg| msg.into_static())
        .map_err(|err| anyhow!("binary sv2 decode: {err:?}"))
}

fn decode_open_extended_channel(
    payload: &[u8],
) -> anyhow::Result<OpenExtendedMiningChannel<'static>> {
    let mut bytes = payload.to_vec();
    binary_sv2::from_bytes::<OpenExtendedMiningChannel<'_>>(&mut bytes)
        .map(|msg| msg.into_static())
        .map_err(|err| anyhow!("binary sv2 decode: {err:?}"))
}

fn decode_submit_extended(payload: &[u8]) -> anyhow::Result<SubmitSharesExtended<'static>> {
    let mut bytes = payload.to_vec();
    binary_sv2::from_bytes::<SubmitSharesExtended<'_>>(&mut bytes)
        .map(|msg| msg.into_static())
        .map_err(|err| anyhow!("binary sv2 decode: {err:?}"))
}

fn str0255(value: &str) -> anyhow::Result<Str0255<'static>> {
    value
        .to_string()
        .try_into()
        .map_err(|err| anyhow!("build Str0255: {err:?}"))
}

fn b032(bytes: Vec<u8>) -> anyhow::Result<B032<'static>> {
    bytes
        .try_into()
        .map_err(|err| anyhow!("build B032: {err:?}"))
}

fn b064k(bytes: Vec<u8>) -> anyhow::Result<B064K<'static>> {
    bytes
        .try_into()
        .map_err(|err| anyhow!("build B064K: {err:?}"))
}

fn u256_from_le(bytes: [u8; 32]) -> U256<'static> {
    U256::from(bytes)
}

fn merkle_path(branches: &[[u8; 32]]) -> anyhow::Result<Seq0255<'static, U256<'static>>> {
    let path = branches.iter().copied().map(U256::from).collect::<Vec<_>>();
    Seq0255::new(path).map_err(|err| anyhow!("build merkle path: {err:?}"))
}

fn new_extended_job_min_ntime(clean: bool, job: &JobTemplate) -> Sv2Option<'static, u32> {
    if clean {
        Sv2Option::new(None)
    } else {
        Sv2Option::new(Some(sv2_min_ntime(job)))
    }
}

fn sv2_min_ntime(job: &JobTemplate) -> u32 {
    u32::from_str_radix(&job.ntime, 16)
        .unwrap_or(job.mintime_u32)
        .max(job.mintime_u32)
}

fn sv2_session_id(extranonce_prefix: &[u8]) -> String {
    let short_id = hex::encode(extranonce_prefix)
        .chars()
        .take(8)
        .collect::<String>();
    format!("sv2-{short_id}")
}

fn sv2_worker_label(user_agent: Option<&str>, session_id: &str) -> String {
    let base = user_agent
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Unknown Miner");
    let short_id = session_id.trim_start_matches("sv2-");
    format!("{base} #{short_id}")
}

fn sv2_initial_difficulty(config: &Config, nominal_hash_rate: f32) -> f64 {
    let suggested = if nominal_hash_rate.is_finite() && nominal_hash_rate > 0.0 {
        Some(nominal_hash_rate as f64 * config.target_share_time_secs / HASHES_PER_DIFF)
    } else {
        None
    };
    sv2_suggested_initial_difficulty(
        suggested,
        config.start_difficulty,
        config.min_difficulty,
        config.max_difficulty,
    )
}

fn sv2_suggested_initial_difficulty(
    suggested: Option<f64>,
    start_difficulty: f64,
    min_difficulty: f64,
    max_difficulty: f64,
) -> f64 {
    suggested
        .filter(|value| value.is_finite() && *value > start_difficulty)
        .unwrap_or(start_difficulty)
        .clamp(min_difficulty, max_difficulty)
}

fn set_channel_difficulty(channel: &mut ChannelState, difficulty: f64) -> anyhow::Result<[u8; 32]> {
    let share_target = share_target_le(difficulty)?;
    channel.difficulty = difficulty;
    channel.share_target_le = share_target;
    for job in channel.jobs.values_mut() {
        job.difficulty = difficulty;
        job.share_target_le = share_target;
    }
    Ok(share_target)
}

fn prune_old_jobs(channel: &mut ChannelState, keep_job_id: u32) {
    channel.jobs.retain(|job_id, _| *job_id == keep_job_id);
}

fn should_clean_channel_jobs(channel: &ChannelState, job: &JobTemplate) -> bool {
    if !job.submit_old {
        return true;
    }

    let Some(current_job_id) = channel.current_job_id else {
        return true;
    };
    let Some(current_job) = channel.jobs.get(&current_job_id) else {
        return true;
    };

    current_job.template.prevhash_le != job.prevhash_le
}

fn version_mask_for_job(job: &JobTemplate) -> u32 {
    ALLOWED_VERSION_ROLLING_MASK & !job.vbrequired
}

fn resolve_submitted_version(channel_job: &ChannelJob, submit_version: u32) -> anyhow::Result<u32> {
    let job_version = channel_job.template.version_u32;
    if !channel_job.version_rolling_allowed {
        if submit_version == job_version {
            return Ok(job_version);
        }
        bail!("sv2 submitted version changed but version rolling was not negotiated");
    }

    let submit_outside = submit_version & !channel_job.version_mask;
    let job_outside = job_version & !channel_job.version_mask;
    if submit_outside != 0 && submit_outside != job_outside {
        bail!("sv2 submitted version has bits outside negotiated mask");
    }

    Ok((job_version & !channel_job.version_mask) | (submit_version & channel_job.version_mask))
}

fn first_u32_le(payload: &[u8]) -> Option<u32> {
    let bytes = payload.get(..4)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn first_two_u32_le(payload: &[u8]) -> (Option<u32>, Option<u32>) {
    let first = first_u32_le(payload);
    let second = payload
        .get(4..8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes);
    (first, second)
}

fn build_dup_key(submit: &SubmitSharesExtended<'_>, resolved_version: u32) -> DupKey {
    let mut extranonce = [0u8; 32];
    let extra = submit.extranonce.inner_as_ref();
    let len = extra.len().min(extranonce.len());
    extranonce[..len].copy_from_slice(&extra[..len]);
    (
        submit.channel_id,
        submit.job_id,
        submit.nonce,
        submit.ntime,
        extranonce,
        len as u8,
        resolved_version,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use common_messages_sv2::Protocol;

    fn setup(flags: u32) -> SetupConnection<'static> {
        SetupConnection {
            protocol: Protocol::MiningProtocol,
            min_version: 2,
            max_version: 2,
            flags,
            endpoint_host: str0255("127.0.0.1").unwrap(),
            endpoint_port: 3334,
            vendor: str0255("test").unwrap(),
            hardware_version: str0255("test").unwrap(),
            firmware: str0255("test").unwrap(),
            device_id: str0255("").unwrap(),
        }
    }

    fn channel_with_current_job(prevhash_le: &str) -> ChannelState {
        let mut template = JobTemplate::empty();
        template.prevhash_le = prevhash_le.to_string();

        let mut jobs = HashMap::new();
        jobs.insert(
            7,
            ChannelJob {
                template: Arc::new(template),
                coinbase_prefix: Arc::new(Vec::new()),
                share_target_le: [0xff; 32],
                difficulty: 1.0,
                created_at: Utc::now(),
                version_rolling_allowed: false,
                version_mask: 0,
            },
        );

        ChannelState {
            worker: "test-worker".to_string(),
            extranonce_prefix: Vec::new(),
            extranonce_size: 4,
            difficulty: 1.0,
            share_target_le: [0xff; 32],
            target_sent: false,
            vardiff: VardiffController::new(15.0, 90.0, 1.0, 1024.0),
            jobs,
            current_job_id: Some(7),
        }
    }

    #[test]
    fn setup_accepts_plain_mining_protocol() {
        assert_eq!(decide_setup(&setup(0)), SetupDecision::Accept { flags: 0 });
    }

    #[test]
    fn setup_rejects_standard_job_requirement() {
        assert_eq!(
            decide_setup(&setup(0b1)),
            SetupDecision::Reject {
                flags: 0b1,
                code: "unsupported-feature-flags"
            }
        );
    }

    #[test]
    fn setup_allows_version_rolling_flag() {
        assert_eq!(
            decide_setup(&setup(0b100)),
            SetupDecision::Accept { flags: 0b100 }
        );
    }

    #[test]
    fn setup_user_agent_uses_downstream_telemetry_not_channel_identity() {
        let mut req = setup(0);
        req.vendor = str0255("Bitaxe").unwrap();
        req.hardware_version = str0255("Gamma").unwrap();
        req.firmware = str0255("2.5.0").unwrap();
        req.device_id = str0255("private-device-id").unwrap();

        assert_eq!(
            setup_connection_user_agent(&req).as_deref(),
            Some("Bitaxe Gamma 2.5.0")
        );
    }

    #[test]
    fn sv2_worker_label_distinguishes_same_user_agent_by_extranonce_prefix() {
        let left_session = sv2_session_id(&[0x69, 0xaa, 0xbb, 0xcc]);
        let right_session = sv2_session_id(&[0xd2, 0xaa, 0xbb, 0xcc]);

        assert_eq!(left_session, "sv2-69aabbcc");
        assert_eq!(right_session, "sv2-d2aabbcc");
        assert_eq!(
            sv2_worker_label(Some("Bitaxe"), &left_session),
            "Bitaxe #69aabbcc"
        );
        assert_ne!(
            sv2_worker_label(Some("Bitaxe"), &left_session),
            sv2_worker_label(Some("Bitaxe"), &right_session)
        );
    }

    #[test]
    fn sv2_suggested_initial_difficulty_matches_sv1_suggest_rules() {
        assert_eq!(
            sv2_suggested_initial_difficulty(Some(32_768.0), 16_384.0, 1_024.0, 65_536.0),
            32_768.0
        );
        assert_eq!(
            sv2_suggested_initial_difficulty(Some(2_000.0), 16_384.0, 1_024.0, 65_536.0),
            16_384.0
        );
        assert_eq!(
            sv2_suggested_initial_difficulty(Some(1_000_000.0), 16_384.0, 1_024.0, 65_536.0),
            65_536.0
        );
        assert_eq!(
            sv2_suggested_initial_difficulty(Some(f64::NAN), 16_384.0, 1_024.0, 65_536.0),
            16_384.0
        );
    }

    #[test]
    fn setup_rejects_version_mismatch() {
        let mut req = setup(0);
        req.min_version = 3;
        req.max_version = 3;

        assert_eq!(
            decide_setup(&req),
            SetupDecision::Reject {
                flags: 0,
                code: "protocol-version-mismatch"
            }
        );
    }

    #[test]
    fn template_update_with_same_prevhash_does_not_clean_sv2_jobs() {
        let channel = channel_with_current_job("prevhash-a");
        let mut update = JobTemplate::empty();
        update.prevhash_le = "prevhash-a".to_string();
        update.submit_old = true;

        assert!(!should_clean_channel_jobs(&channel, &update));
    }

    #[test]
    fn sv2_jobs_are_cleaned_for_new_prevhash_or_submitold_false() {
        let channel = channel_with_current_job("prevhash-a");
        let mut update = JobTemplate::empty();
        update.prevhash_le = "prevhash-b".to_string();
        update.submit_old = true;
        assert!(should_clean_channel_jobs(&channel, &update));

        update.prevhash_le = "prevhash-a".to_string();
        update.submit_old = false;
        assert!(should_clean_channel_jobs(&channel, &update));
    }

    #[test]
    fn sv2_channel_retarget_updates_cached_jobs() {
        let mut channel = channel_with_current_job("prevhash-a");
        let target = set_channel_difficulty(&mut channel, 4.0).unwrap();

        assert_eq!(channel.difficulty, 4.0);
        assert_eq!(channel.share_target_le, target);
        let job = channel.jobs.get(&7).unwrap();
        assert_eq!(job.difficulty, 4.0);
        assert_eq!(job.share_target_le, target);
    }

    #[test]
    fn sv2_non_clean_updates_are_immediate_jobs_without_prevhash_reset() {
        let mut job = JobTemplate::empty();
        job.ntime = "6a1e4be0".to_string();
        job.mintime_u32 = 0x6a1e_4bda;

        assert_eq!(
            new_extended_job_min_ntime(false, &job).into_inner(),
            Some(0x6a1e_4be0)
        );
        assert_eq!(new_extended_job_min_ntime(true, &job).into_inner(), None);
    }

    #[test]
    fn sv2_min_ntime_never_drops_below_template_mintime() {
        let mut job = JobTemplate::empty();
        job.ntime = "6a1e4bd0".to_string();
        job.mintime_u32 = 0x6a1e_4bda;

        assert_eq!(sv2_min_ntime(&job), 0x6a1e_4bda);
    }

    #[test]
    fn duplicate_key_ignores_sequence_number() {
        let a = SubmitSharesExtended {
            channel_id: 1,
            sequence_number: 1,
            job_id: 2,
            nonce: 3,
            ntime: 4,
            version: 0x2000_0000,
            extranonce: b032(vec![1, 2, 3, 4]).unwrap(),
        };
        let mut b = a.clone();
        b.sequence_number = 2;

        assert_eq!(build_dup_key(&a, a.version), build_dup_key(&b, b.version));

        b.nonce = 5;
        assert_ne!(build_dup_key(&a, a.version), build_dup_key(&b, b.version));
    }

    #[test]
    fn duplicate_key_keeps_high_extranonce_bytes_distinct() {
        let base = SubmitSharesExtended {
            channel_id: 1,
            sequence_number: 1,
            job_id: 2,
            nonce: 3,
            ntime: 4,
            version: 0x2000_0000,
            extranonce: b032(vec![0; 32]).unwrap(),
        };
        let mut other = base.clone();
        let mut bytes = vec![0; 32];
        bytes[31] = 1;
        other.extranonce = b032(bytes).unwrap();

        assert_ne!(
            build_dup_key(&base, base.version),
            build_dup_key(&other, other.version)
        );
    }

    #[test]
    fn submitted_version_must_match_when_rolling_not_negotiated() {
        let mut template = JobTemplate::empty();
        template.version_u32 = 0x2000_0000;
        let channel_job = ChannelJob {
            template: Arc::new(template),
            coinbase_prefix: Arc::new(Vec::new()),
            share_target_le: [0xff; 32],
            difficulty: 1.0,
            created_at: Utc::now(),
            version_rolling_allowed: false,
            version_mask: 0,
        };

        assert_eq!(
            resolve_submitted_version(&channel_job, 0x2000_0000).unwrap(),
            0x2000_0000
        );
        assert!(resolve_submitted_version(&channel_job, 0x2000_2000).is_err());
    }

    #[test]
    fn submitted_version_is_capped_to_rolling_mask() {
        let mut template = JobTemplate::empty();
        template.version_u32 = 0x2000_0000;
        template.vbrequired = 0x0000_2000;
        let channel_job = ChannelJob {
            version_mask: version_mask_for_job(&template),
            template: Arc::new(template),
            coinbase_prefix: Arc::new(Vec::new()),
            share_target_le: [0xff; 32],
            difficulty: 1.0,
            created_at: Utc::now(),
            version_rolling_allowed: true,
        };

        assert_eq!(channel_job.version_mask, 0x1fff_c000);
        assert_eq!(
            resolve_submitted_version(&channel_job, 0x2000_4000).unwrap(),
            0x2000_4000
        );
        assert!(resolve_submitted_version(&channel_job, 0x2000_2000).is_err());
    }
}
