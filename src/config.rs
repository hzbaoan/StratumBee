use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use bitcoin::Network;
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct Config {
    pub network: Network,
    pub stratum_bind: String,
    pub stratum_port: u16,
    pub api_bind: String,
    pub api_port: u16,
    pub api_enabled: bool,
    pub rpc_url: String,
    pub rpc_user: String,
    pub rpc_pass: String,
    pub zmq_block_urls: Vec<String>,
    pub p2p_fast_peer: Option<String>,
    pub payout_address: String,
    pub payout_script_hex: Option<String>,
    pub pool_tag: String,
    pub coinbase_message: String,
    pub extranonce1_size: usize,
    pub extranonce2_size: usize,
    pub min_difficulty: f64,
    pub max_difficulty: f64,
    pub start_difficulty: f64,
    pub target_share_time_secs: f64,
    pub vardiff_retarget_time_secs: f64,
    pub vardiff_enabled: bool,
    pub notify_bucket_capacity: f64,
    pub notify_bucket_refill_ms: f64,
    pub auth_token: Option<String>,
    pub max_line_bytes: usize,
    pub idle_timeout_secs: u64,
    pub miner_inactive_timeout_secs: u64,
    pub max_recent_events: usize,
    pub max_block_history: usize,
    pub save_solved_blocks_dir: Option<PathBuf>,
    pub block_archive_pre_submit: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    bitcoin: BitcoinSection,
    stratum: StratumSection,
    api: ApiSection,
    mining: MiningSection,
    runtime: RuntimeSection,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BitcoinSection {
    network: String,
    rpc_url: String,
    rpc_user: String,
    rpc_pass: String,
    zmq_block_urls: Vec<String>,
    p2p_fast_peer: Option<String>,
}

impl Default for BitcoinSection {
    fn default() -> Self {
        Self {
            network: "mainnet".to_string(),
            rpc_url: String::new(),
            rpc_user: String::new(),
            rpc_pass: String::new(),
            zmq_block_urls: vec![],
            p2p_fast_peer: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StratumSection {
    bind: String,
    port: u16,
    extranonce1_size: usize,
    extranonce2_size: usize,
    min_difficulty: f64,
    max_difficulty: f64,
    start_difficulty: f64,
    target_share_time_secs: f64,
    vardiff_retarget_time_secs: f64,
    vardiff_enabled: bool,
    auth_token: Option<String>,
    max_line_bytes: usize,
    idle_timeout_secs: u64,
}

impl Default for StratumSection {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".to_string(),
            port: 3333,
            extranonce1_size: 4,
            extranonce2_size: 4,
            min_difficulty: 16_384.0,
            max_difficulty: 4_194_304.0,
            start_difficulty: 16_384.0,
            target_share_time_secs: 15.0,
            vardiff_retarget_time_secs: 90.0,
            vardiff_enabled: true,
            auth_token: None,
            max_line_bytes: 65_536,
            idle_timeout_secs: 300,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ApiSection {
    bind: String,
    port: u16,
    enabled: bool,
}

impl Default for ApiSection {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: 8080,
            enabled: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MiningSection {
    payout_address: String,
    payout_script_hex: Option<String>,
    pool_tag: String,
    coinbase_message: String,
    save_solved_blocks_dir: Option<String>,
    block_archive_pre_submit: bool,
}

impl Default for MiningSection {
    fn default() -> Self {
        Self {
            payout_address: String::new(),
            payout_script_hex: None,
            pool_tag: "StratumBee".to_string(),
            coinbase_message: "Solo".to_string(),
            save_solved_blocks_dir: Some("var/solved-blocks".to_string()),
            block_archive_pre_submit: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RuntimeSection {
    notify_bucket_capacity: f64,
    notify_bucket_refill_ms: f64,
    miner_inactive_timeout_secs: u64,
    max_recent_events: usize,
    max_block_history: usize,
}

impl Default for RuntimeSection {
    fn default() -> Self {
        Self {
            notify_bucket_capacity: 2.0,
            notify_bucket_refill_ms: 500.0,
            miner_inactive_timeout_secs: 1_800,
            max_recent_events: 1_024,
            max_block_history: 32,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?;
        let file_cfg = parse_file_config(&raw)?;

        let network = parse_network(&env_or("STRATUMBEE_NETWORK", &file_cfg.bitcoin.network))?;
        let rpc_url = required_env_or("STRATUMBEE_RPC_URL", &file_cfg.bitcoin.rpc_url, "rpc_url")?;
        let rpc_user = required_env_or("STRATUMBEE_RPC_USER", &file_cfg.bitcoin.rpc_user, "rpc_user")?;
        let rpc_pass = required_env_or("STRATUMBEE_RPC_PASS", &file_cfg.bitcoin.rpc_pass, "rpc_pass")?;
        let payout_script_hex = normalize_opt(
            env::var("STRATUMBEE_PAYOUT_SCRIPT_HEX").ok().as_deref(),
            file_cfg.mining.payout_script_hex.as_deref(),
        );
        let payout_address = if payout_script_hex.is_some() {
            env_or("STRATUMBEE_PAYOUT_ADDRESS", &file_cfg.mining.payout_address)
        } else {
            required_env_or(
                "STRATUMBEE_PAYOUT_ADDRESS",
                &file_cfg.mining.payout_address,
                "payout_address",
            )?
        };

        let min_difficulty = file_cfg.stratum.min_difficulty;
        let max_difficulty = file_cfg.stratum.max_difficulty.max(min_difficulty);
        let start_difficulty = file_cfg
            .stratum
            .start_difficulty
            .clamp(min_difficulty, max_difficulty);

        let cfg = Self {
            network,
            stratum_bind: file_cfg.stratum.bind,
            stratum_port: file_cfg.stratum.port,
            api_bind: file_cfg.api.bind,
            api_port: file_cfg.api.port,
            api_enabled: file_cfg.api.enabled,
            rpc_url,
            rpc_user,
            rpc_pass,
            zmq_block_urls: file_cfg.bitcoin.zmq_block_urls,
            p2p_fast_peer: normalize_opt(
                env::var("STRATUMBEE_P2P_FAST_PEER").ok().as_deref(),
                file_cfg.bitcoin.p2p_fast_peer.as_deref(),
            ),
            payout_address,
            payout_script_hex,
            pool_tag: file_cfg.mining.pool_tag,
            coinbase_message: file_cfg.mining.coinbase_message,
            extranonce1_size: file_cfg.stratum.extranonce1_size,
            extranonce2_size: file_cfg.stratum.extranonce2_size,
            min_difficulty,
            max_difficulty,
            start_difficulty,
            target_share_time_secs: file_cfg.stratum.target_share_time_secs.max(1.0),
            vardiff_retarget_time_secs: file_cfg.stratum.vardiff_retarget_time_secs.max(5.0),
            vardiff_enabled: file_cfg.stratum.vardiff_enabled,
            notify_bucket_capacity: file_cfg.runtime.notify_bucket_capacity.max(1.0),
            notify_bucket_refill_ms: file_cfg.runtime.notify_bucket_refill_ms.max(10.0),
            auth_token: normalize_opt(
                env::var("STRATUMBEE_AUTH_TOKEN").ok().as_deref(),
                file_cfg.stratum.auth_token.as_deref(),
            ),
            max_line_bytes: file_cfg.stratum.max_line_bytes.max(1_024),
            idle_timeout_secs: file_cfg.stratum.idle_timeout_secs.max(30),
            miner_inactive_timeout_secs: file_cfg.runtime.miner_inactive_timeout_secs.max(30),
            max_recent_events: file_cfg.runtime.max_recent_events.max(128),
            max_block_history: file_cfg.runtime.max_block_history.max(8),
            save_solved_blocks_dir: file_cfg.mining.save_solved_blocks_dir.map(PathBuf::from),
            block_archive_pre_submit: file_cfg.mining.block_archive_pre_submit,
        };

        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.extranonce1_size == 0 || self.extranonce1_size > 16 {
            bail!("extranonce1_size must be between 1 and 16");
        }
        if self.extranonce2_size == 0 || self.extranonce2_size > 16 {
            bail!("extranonce2_size must be between 1 and 16");
        }
        if !self.min_difficulty.is_finite() || self.min_difficulty <= 0.0 {
            bail!("stratum.min_difficulty must be finite and > 0");
        }
        if !self.max_difficulty.is_finite() || self.max_difficulty < self.min_difficulty {
            bail!("stratum.max_difficulty must be finite and >= min_difficulty");
        }
        if !self.start_difficulty.is_finite()
            || self.start_difficulty < self.min_difficulty
            || self.start_difficulty > self.max_difficulty
        {
            bail!("stratum.start_difficulty must be finite and within min/max difficulty");
        }
        if self.stratum_port == 0 {
            bail!("stratum.port must be > 0");
        }
        if self.api_enabled && self.api_port == 0 {
            bail!("api.port must be > 0 when api.enabled = true");
        }
        if self.payout_script_hex.is_none() && self.payout_address.trim().is_empty() {
            bail!("mining.payout_address is required when payout_script_hex is empty");
        }
        if self.pool_tag.len() > 32 {
            bail!("mining.pool_tag is too long; keep it under 32 bytes");
        }
        if self.coinbase_message.len() > 32 {
            bail!("mining.coinbase_message is too long; keep it under 32 bytes");
        }
        if self.rpc_url.trim().is_empty() {
            bail!("bitcoin.rpc_url is required");
        }
        if self.rpc_user.trim().is_empty() {
            bail!("bitcoin.rpc_user is required");
        }
        if self.rpc_pass.trim().is_empty() {
            bail!("bitcoin.rpc_pass is required");
        }
        Ok(())
    }
}

fn parse_network(raw: &str) -> anyhow::Result<Network> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "mainnet" | "bitcoin" => Ok(Network::Bitcoin),
        "testnet" => Ok(Network::Testnet),
        "regtest" => Ok(Network::Regtest),
        "signet" => Ok(Network::Signet),
        other => Err(anyhow!("unsupported network: {other}")),
    }
}

fn env_or(var: &str, fallback: &str) -> String {
    env::var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.trim().to_string())
}

fn required_env_or(var: &str, fallback: &str, label: &str) -> anyhow::Result<String> {
    let value = env_or(var, fallback);
    if value.is_empty() {
        return Err(anyhow!("{label} is required"));
    }
    Ok(value)
}

fn normalize_opt(primary: Option<&str>, fallback: Option<&str>) -> Option<String> {
    primary
        .and_then(trim_non_empty)
        .or_else(|| fallback.and_then(trim_non_empty))
}

fn trim_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_file_config(raw: &str) -> anyhow::Result<FileConfig> {
    toml::from_str(raw).context("parse TOML config")
}

#[cfg(test)]
mod tests {
    use bitcoin::Network;

    use super::{parse_file_config, parse_network};

    #[test]
    fn rejects_unknown_runtime_field() {
        let raw = r#"
[runtime]
unexpected = 1000
"#;
        let err = parse_file_config(raw).expect_err("unknown runtime field should be rejected");
        let err_text = format!("{err:#}");
        assert!(err_text.contains("unknown field"));
        assert!(err_text.contains("unexpected"));
    }

    #[test]
    fn accepts_regtest_network_name() {
        let network = parse_network("regtest").expect("regtest should parse");
        assert_eq!(network, Network::Regtest);
    }
}
