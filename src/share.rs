use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use num_bigint::BigUint;
use num_traits::{Num, ToPrimitive};

use crate::hash::{double_sha256, merkle_step};
use crate::template::JobTemplate;

const DIFF1_TARGET_HEX: &str =
    "00000000ffff0000000000000000000000000000000000000000000000000000";

pub fn share_target_le(diff: f64) -> anyhow::Result<[u8; 32]> {
    if !diff.is_finite() || diff <= 0.0 {
        return Err(anyhow!("diff must be finite and > 0"));
    }
    const SCALE: u128 = 1_000_000_000_000;
    let diff_scaled = ((diff * SCALE as f64).round() as u128).max(1);

    let mut diff1 = BigUint::from_str_radix(DIFF1_TARGET_HEX, 16)?;
    diff1 *= BigUint::from(SCALE);
    let target = diff1 / BigUint::from(diff_scaled);

    let be = target.to_bytes_be();
    if be.len() > 32 {
        return Ok([0xffu8; 32]);
    }

    let mut out = vec![0u8; 32 - be.len()];
    out.extend_from_slice(&be);
    out.reverse();
    Ok(out.try_into().expect("target must be 32 bytes"))
}

#[derive(Debug, Clone)]
pub struct ShareSubmit {
    pub extranonce2: String,
    pub ntime: String,
    pub nonce: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ShareResult {
    pub accepted: bool,
    pub is_block: bool,
    pub hash_hex: String,
    pub block_hex: Option<String>,
    pub coinbase_hex: Option<String>,
    pub difficulty: f64,
}

pub fn validate_share(
    job: &JobTemplate,
    coinbase_prefix: &[u8],
    submit: &ShareSubmit,
    share_target_le: &[u8; 32],
    coinbase2_override: Option<&[u8]>,
) -> anyhow::Result<ShareResult> {
    let ntime_u32 = parse_u32_be(&submit.ntime)?;
    if job.mintime_u32 != 0 && ntime_u32 < job.mintime_u32 {
        return Ok(reject());
    }
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if (ntime_u32 as u64) > now_secs.saturating_add(7200) {
        return Ok(reject());
    }

    let en2 = hex::decode(submit.extranonce2.trim_start_matches("0x"))
        .context("decode extranonce2")?;
    let coinbase2 = coinbase2_override.unwrap_or(&job.coinbase2_bytes);
    let mut coinbase = Vec::with_capacity(coinbase_prefix.len() + en2.len() + coinbase2.len());
    coinbase.extend_from_slice(coinbase_prefix);
    coinbase.extend_from_slice(&en2);
    coinbase.extend_from_slice(coinbase2);

    let coinbase_hash = double_sha256(&coinbase);
    let mut merkle_root = coinbase_hash;
    for branch in &job.merkle_branches_le {
        merkle_root = merkle_step(&merkle_root, branch);
    }

    let version_u32 = submit
        .version
        .as_deref()
        .and_then(|value| parse_u32_be(value).ok())
        .unwrap_or(job.version_u32);
    let nonce_u32 = parse_u32_be(&submit.nonce)?;

    let header = build_header(
        version_u32,
        &job.prevhash_le_bytes,
        &merkle_root,
        ntime_u32,
        job.nbits_u32,
        nonce_u32,
    );

    let hash_le = double_sha256(&header);
    let accepted = leq_le256(&hash_le, share_target_le);
    let is_block = leq_le256(&hash_le, &job.target_le);

    let mut hash_display = hash_le;
    hash_display.reverse();
    let hash_hex = hex::encode(hash_display);
    let difficulty = hash_to_display_diff(&hash_le);

    let (block_hex, coinbase_hex) = if is_block {
        (
            Some(build_block_hex(
                &header,
                &coinbase,
                &job.transactions,
                job.has_witness_commitment,
            )?),
            Some(hex::encode(&coinbase)),
        )
    } else {
        (None, None)
    };

    Ok(ShareResult {
        accepted,
        is_block,
        hash_hex,
        block_hex,
        coinbase_hex,
        difficulty,
    })
}

fn reject() -> ShareResult {
    ShareResult {
        accepted: false,
        is_block: false,
        hash_hex: "0".repeat(64),
        block_hex: None,
        coinbase_hex: None,
        difficulty: 0.0,
    }
}

#[inline]
pub fn leq_le256(a: &[u8; 32], b: &[u8; 32]) -> bool {
    for idx in (0..32).rev() {
        if a[idx] < b[idx] {
            return true;
        }
        if a[idx] > b[idx] {
            return false;
        }
    }
    true
}

fn hash_to_display_diff(hash_le: &[u8; 32]) -> f64 {
    let diff1 = match BigUint::from_str_radix(DIFF1_TARGET_HEX, 16) {
        Ok(v) => v,
        Err(_) => return 0.0,
    };
    let mut be = *hash_le;
    be.reverse();
    let hash_val = BigUint::from_bytes_be(&be);
    if hash_val == BigUint::ZERO {
        return f64::INFINITY;
    }
    let quotient = &diff1 / &hash_val;
    let remainder = &diff1 % &hash_val;
    let q_f64 = quotient.to_f64().unwrap_or(f64::MAX);
    let frac = match (remainder.to_f64(), hash_val.to_f64()) {
        (Some(r), Some(h)) if h > 0.0 => r / h,
        _ => 0.0,
    };
    q_f64 + frac
}

#[inline]
fn build_header(
    version: u32,
    prevhash_le: &[u8; 32],
    merkle_root: &[u8; 32],
    ntime: u32,
    nbits: u32,
    nonce: u32,
) -> [u8; 80] {
    let mut h = [0u8; 80];
    h[0..4].copy_from_slice(&version.to_le_bytes());
    h[4..36].copy_from_slice(prevhash_le);
    h[36..68].copy_from_slice(merkle_root);
    h[68..72].copy_from_slice(&ntime.to_le_bytes());
    h[72..76].copy_from_slice(&nbits.to_le_bytes());
    h[76..80].copy_from_slice(&nonce.to_le_bytes());
    h
}

fn parse_u32_be(s: &str) -> anyhow::Result<u32> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_| anyhow!("invalid u32 hex"))
}

fn build_block_hex(
    header: &[u8; 80],
    coinbase_legacy: &[u8],
    txs_hex: &[String],
    use_witness: bool,
) -> anyhow::Result<String> {
    let mut block = Vec::new();
    block.extend_from_slice(header);
    encode_varint(txs_hex.len() as u64 + 1, &mut block);
    let coinbase = if use_witness {
        build_coinbase_with_witness(coinbase_legacy)?
    } else {
        coinbase_legacy.to_vec()
    };
    block.extend_from_slice(&coinbase);
    for tx_hex in txs_hex {
        block.extend_from_slice(&hex::decode(tx_hex).context("decode transaction")?);
    }
    Ok(hex::encode(block))
}

fn build_coinbase_with_witness(legacy: &[u8]) -> anyhow::Result<Vec<u8>> {
    if legacy.len() < 8 {
        return Err(anyhow!("coinbase tx too short"));
    }
    let (version, rest) = legacy.split_at(4);
    let (core, locktime) = rest.split_at(rest.len() - 4);
    let mut out = Vec::with_capacity(legacy.len() + 36);
    out.extend_from_slice(version);
    out.push(0x00);
    out.push(0x01);
    out.extend_from_slice(core);
    out.push(0x01);
    out.push(0x20);
    out.extend_from_slice(&[0u8; 32]);
    out.extend_from_slice(locktime);
    Ok(out)
}

fn encode_varint(v: u64, out: &mut Vec<u8>) {
    match v {
        0..=0xfc => out.push(v as u8),
        0xfd..=0xffff => {
            out.push(0xfd);
            out.extend_from_slice(&(v as u16).to_le_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(0xfe);
            out.extend_from_slice(&(v as u32).to_le_bytes());
        }
        _ => {
            out.push(0xff);
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;
    use num_traits::Num;

    use super::{hash_to_display_diff, leq_le256, DIFF1_TARGET_HEX};

    #[test]
    fn equal_threshold_is_accepted() {
        let threshold = [0x11u8; 32];
        assert!(leq_le256(&threshold, &threshold));
    }

    #[test]
    fn true_diff_is_not_capped() {
        let diff1 = BigUint::from_str_radix(DIFF1_TARGET_HEX, 16).unwrap();
        let target_big = &diff1 / BigUint::from(1_000_000u64);
        let mut target_be = target_big.to_bytes_be();
        while target_be.len() < 32 {
            target_be.insert(0, 0);
        }
        let mut hash_le: [u8; 32] = target_be.try_into().unwrap();
        hash_le.reverse();
        let true_diff = hash_to_display_diff(&hash_le);
        assert!(true_diff > 900_000.0);
    }
}
