# StratumBee

StratumBee is a lightweight solo Bitcoin Stratum V1 gateway written in Rust for VPS instances, Raspberry Pi class hardware, and other low-resource hosts.

It bridges `bitcoind` `getblocktemplate`, longpoll, optional ZMQ, and an optional Bitcoin P2P fast-block feed to Stratum V1 miners. Share validation, full block assembly, candidate block archiving, `submitblock`, and read-only monitoring all stay local.

## Scope

StratumBee is a good fit when:

- You already run a Bitcoin Core node and want a lightweight solo mining entrypoint.
- You do not need pool accounts, a database, payout accounting, or balance management.
- You want share validation and block submission to happen locally instead of delegating to an upstream pool.
- You want something that can run in a constrained environment and still expose a read-only dashboard.

StratumBee does not do the following:

- Pool payout splitting or accounting
- Dynamic payout address switching based on miner username
- Write-capable management APIs
- Stratum V2

## Current Capabilities

- Pulls templates from `getblocktemplate` and uses longpoll as the primary update channel
- Listens to `hashblock` and `rawblock` ZMQ events to refresh faster after new blocks
- Supports `bitcoin.p2p_fast_peer` and reacts to `headers`, `cmpctblock`, and `inv` announcements to push clean jobs earlier
- Serves Stratum V1 with `subscribe`, `authorize`, `submit`, `suggest_difficulty`, `configure`, `get_version`, and `ping`
- Supports version rolling with a negotiated mask capped at `1fffe000`
- Validates shares locally and computes both real share difficulty and full-network target hits
- Builds full block hex locally before calling `submitblock`
- Treats `submitblock` responses of `duplicate` as success
- Optionally archives solved block candidates to local JSON files
- Keeps miner state, share events, and recent block history in memory with no external database
- Exposes a built-in read-only dashboard and JSON API
- Supports vardiff based on a target share interval

## How It Works

1. On startup, StratumBee fetches the first `getblocktemplate`.
2. After that it mainly waits on longpoll; if ZMQ is configured, `hashblock` and `rawblock` also trigger refreshes.
3. If `bitcoin.p2p_fast_peer` is configured, `headers`, `cmpctblock`, and similar announcements can trigger an early clean job with a minimal coinbase-only template.
4. The final transaction list, fees, and witness commitment still come from the next real GBT or longpoll template.
5. When a miner submits a share, StratumBee rebuilds the coinbase, merkle root, and block header locally, then validates the share target and block target.
6. If the share solves a block, StratumBee can archive the candidate, submit it to Bitcoin Core, and record the result as `candidate`, `submitted`, `duplicate`, `rejected`, or `submit_failed`.

## Requirements

### Software

- Rust `1.85` or newer
- A Bitcoin Core node with RPC enabled
- ZMQ enabled in Bitcoin Core if you want `hashblock` and `rawblock` notifications
- Reachability to a Bitcoin P2P port if you enable `bitcoin.p2p_fast_peer`
- ZeroMQ development libraries when building on Linux

### Debian / Ubuntu Packages

```bash
apt-get update
apt-get install -y build-essential pkg-config libzmq3-dev
```

## Bitcoin Core Setup

RPC is required. ZMQ is optional but recommended for faster block detection.

```ini
server=1
rpcuser=bitcoinrpc
rpcpassword=change-me
rpcallowip=127.0.0.1

zmqpubhashblock=tcp://127.0.0.1:28334
zmqpubrawblock=tcp://127.0.0.1:28332
```

Notes:

- `zmqpubhashblock` and `zmqpubrawblock` are optional, but recommended.
- Without ZMQ, StratumBee relies on the initial GBT fetch and longpoll.
- With ZMQ enabled, the update path becomes `longpoll + ZMQ`.
- `p2p_fast_peer` does not replace GBT refreshes; it only helps send a clean job earlier after a block announcement.
- If `p2p_fast_peer` is set without an explicit port, StratumBee uses the network default: mainnet `8333`, testnet `18333`, signet `38333`, regtest `18444`.

## Quick Start

1. Edit [`config/stratumbee.toml`](config/stratumbee.toml).
2. Set the Bitcoin Core RPC URL, username, and password.
3. Set `mining.payout_address`, or use `mining.payout_script_hex` if you want to provide the output script directly.
4. Enable `bitcoin.zmq_block_urls` and `bitcoin.p2p_fast_peer` if you want faster block switchovers.
5. Start the service.

Run in development:

```bash
cargo run --release --locked -- --config config/stratumbee.toml
```

Build and run:

```bash
cargo build --release --locked
./target/release/stratumbee --config config/stratumbee.toml
```

This repository includes [`Cargo.lock`](Cargo.lock). Use `--locked` locally, in CI, and in container builds to keep dependency resolution reproducible.

Default ports:

- Stratum: `3333`
- API / Dashboard: `8080`

## Configuration

The sample configuration lives at [`config/stratumbee.toml`](config/stratumbee.toml).

The TOML loader is strict. Unknown sections and unknown keys are rejected. Omit optional fields you are not using instead of keeping stale placeholders.

### `[bitcoin]`

| Key | Description |
| --- | --- |
| `network` | `mainnet`, `bitcoin` (alias), `testnet`, `signet`, or `regtest` |
| `rpc_url` | Bitcoin Core RPC endpoint |
| `rpc_user` | RPC username |
| `rpc_pass` | RPC password |
| `zmq_block_urls` | Optional list of ZMQ endpoints |
| `p2p_fast_peer` | Optional fast block announcement peer, as `host` or `host:port` |

### `[stratum]`

| Key | Description |
| --- | --- |
| `bind` / `port` | Stratum listen address and port |
| `extranonce1_size` | `extranonce1` size in bytes, range `1..=16` |
| `extranonce2_size` | `extranonce2` size in bytes, range `1..=16` |
| `min_difficulty` | Minimum share difficulty |
| `max_difficulty` | Maximum share difficulty |
| `start_difficulty` | Initial connection difficulty, clamped between min and max |
| `target_share_time_secs` | Vardiff target share interval |
| `vardiff_retarget_time_secs` | Vardiff retarget interval |
| `vardiff_enabled` | Enables or disables vardiff |
| `auth_token` | Optional password token; when set, miners must send it in the `mining.authorize` password field |
| `max_line_bytes` | Maximum size of one Stratum line |
| `idle_timeout_secs` | Idle connection timeout |

### `[api]`

| Key | Description |
| --- | --- |
| `bind` / `port` | API listen address and port |
| `enabled` | Enables or disables the read-only dashboard and API |

### `[mining]`

| Key | Description |
| --- | --- |
| `payout_address` | Fixed payout address for solved blocks |
| `payout_script_hex` | Optional raw output script; when set, address parsing is not required |
| `pool_tag` | Coinbase pool tag, maximum 32 bytes |
| `coinbase_message` | Extra coinbase message, maximum 32 bytes |
| `save_solved_blocks_dir` | Directory used for solved block candidate archives |
| `block_archive_pre_submit` | Archives the candidate before `submitblock` when enabled |

### `[runtime]`

| Key | Description |
| --- | --- |
| `notify_bucket_capacity` | Token bucket capacity for non-clean-job notifications |
| `notify_bucket_refill_ms` | Token refill interval for those notifications |
| `miner_inactive_timeout_secs` | Removes inactive miners from the dashboard after this many seconds |
| `max_recent_events` | Maximum number of recent share events kept in memory |
| `max_block_history` | Maximum number of recent block records kept in memory |

### Environment Variable Overrides

The following settings can be overridden with environment variables:

```bash
STRATUMBEE_NETWORK
STRATUMBEE_RPC_URL
STRATUMBEE_RPC_USER
STRATUMBEE_RPC_PASS
STRATUMBEE_PAYOUT_ADDRESS
STRATUMBEE_PAYOUT_SCRIPT_HEX
STRATUMBEE_P2P_FAST_PEER
STRATUMBEE_AUTH_TOKEN
```

Typical usage:

```bash
export STRATUMBEE_RPC_URL=http://127.0.0.1:8332
export STRATUMBEE_RPC_USER=bitcoinrpc
export STRATUMBEE_RPC_PASS=change-me
export STRATUMBEE_PAYOUT_ADDRESS=bc1qreplace-with-your-address
```

## Miner Connection

Miner endpoint:

```text
stratum+tcp://YOUR_HOST:3333
```

Rules:

- Payouts always go to `mining.payout_address` or `mining.payout_script_hex`.
- Miner usernames do not change the payout destination.
- If `stratum.auth_token` is not set, `mining.authorize` usually succeeds.
- If `stratum.auth_token` is set, miners must pass that token in the password field.
- The dashboard identifies miners by `user_agent + session suffix`, not by Bitcoin address.

## Supported Stratum Behavior

Implemented methods:

- `mining.subscribe`
- `mining.extranonce.subscribe`
- `mining.authorize`
- `mining.submit`
- `mining.suggest_difficulty`
- `mining.configure`
- `mining.get_version`
- `mining.ping`

Additional notes:

- `mining.configure` supports version rolling and caps the negotiated mask at `1fffe000`.
- Share deduplication uses `job_id + nonce + ntime + extranonce2 + version`.
- Stale shares are classified as new-block switch, expired job, or short reconnect cases.

## Dashboard and API

When `api.enabled = true`, StratumBee starts a read-only dashboard and JSON API on `127.0.0.1:8080` by default.

### Page Routes

- `/`: built-in dashboard
- `/health`: health endpoint; returns `503` until a template is ready

### JSON Endpoints

| Path | Description |
| --- | --- |
| `/api/stats` | Basic stats, template state, and estimated hashrate |
| `/api/workers` | Simplified worker list |
| `/api/v1/summary` | Summary metrics, submission path state, and best share |
| `/api/v1/miners` | Full miner state |
| `/api/v1/blocks` | Recent candidate and submitted block records |
| `/api/v1/events` | Recent share events from the last 30 minutes |
| `/api/v1/network` | Network data from `getmininginfo` |
| `/api/v1/template` | Current template details |
| `/api/v1/mempool` | Mempool data from `getmempoolinfo` |
| `/api/v1/latency` | One-shot TCP RTT samples for currently connected miners |

Notes:

- All API endpoints are read-only.
- CORS is currently fully open. Keep `api.bind` on `127.0.0.1` unless you know exactly how you want to expose it.
- If you need external access, place it behind your own reverse proxy.

## Docker

The repository includes [`Dockerfile`](Dockerfile), [`.dockerignore`](.dockerignore), [`docker-compose.yml`](docker-compose.yml), and [`.github/workflows/docker.yml`](.github/workflows/docker.yml).

Start with:

```bash
docker compose up -d --build
```

Current container setup:

- Multi-stage `Dockerfile`
- Build stage based on `rust:1.85-bookworm`
- Runtime stage based on `debian:bookworm-slim`
- `cargo build --release --locked` in the build stage
- `config/` and `assets/` copied into the runtime image
- Ports `3333` and `8080` exposed
- `network_mode: host` in the Compose example
- `./config` mounted to `/opt/stratumbee/config`
- `./var` mounted to `/opt/stratumbee/var`
- `RUST_LOG`, RPC username, and RPC password injected through environment variables

Image workflow:

- [`.github/workflows/docker.yml`](.github/workflows/docker.yml) runs manually via `workflow_dispatch`
- Images are pushed to `ghcr.io/<owner>/<repo>`
- The default tags are `latest` and the commit SHA
- The target platform is `linux/amd64`

The provided Compose file is mainly aimed at Linux hosts. Docker Desktop users will likely need to adjust networking.

## systemd

See [`deploy/systemd/stratumbee.service`](deploy/systemd/stratumbee.service).

The service template assumes:

- Working directory: `/opt/stratumbee`
- Config file: `/opt/stratumbee/config/stratumbee.toml`
- Optional env file: `/opt/stratumbee/config/stratumbee.env`
- Writable runtime directory: `/opt/stratumbee/var`
- Runtime user: `stratumbee:stratumbee`

Before deployment, make sure:

- The `stratumbee` user and group exist
- The binary is installed at `/usr/local/bin/stratumbee`
- The config file and `var` directory are already in place

## Solved Block Archive

When a submitted share meets the full network target, StratumBee can write the candidate block to a local JSON file before submission.

Default directory:

```text
var/solved-blocks
```

Archive contents include:

- Block height
- Block hash
- Template identifier
- `block_hex`
- `coinbase_hex`
- Save timestamp

## Repository Layout

| Path | Description |
| --- | --- |
| `src/main.rs` | Entry point, configuration loading, and service startup |
| `src/config.rs` | Strict TOML loading, environment overrides, and config validation |
| `src/template.rs` | GBT, longpoll, ZMQ, fast empty templates, coinbase assembly, and `submitblock` |
| `src/stratum.rs` | Stratum V1 sessions, notify flow, submit handling, version rolling, and vardiff |
| `src/share.rs` | Local share and block validation plus full block assembly |
| `src/metrics.rs` | In-memory metrics, share events, and block history |
| `src/api.rs` | Dashboard and read-only JSON API |
| `src/p2p.rs` | Bitcoin P2P fast block announcement client |
| `src/rpc.rs` | Bitcoin Core RPC and longpoll client |
| `src/hash.rs` | Hashing and merkle helpers |
| `src/vardiff.rs` | Vardiff controller |
| `assets/dashboard.html` | Single-file dashboard |
| `config/stratumbee.toml` | Sample configuration |
| `Cargo.lock` | Locked dependency graph for `--locked` builds |
| `.dockerignore` | Container build context exclusions |
| `Dockerfile` | Container image build |
| `docker-compose.yml` | Container deployment example |
| `.github/workflows/docker.yml` | Manual GHCR image build and push workflow |
| `deploy/systemd/stratumbee.service` | systemd unit template |

## Known Limits

- Metrics and events live only in memory and are lost on restart
- TLS termination is not included; use a reverse proxy if you need public exposure
- There is no miner account system and no payout accounting
- The deployment model is currently centered on a single node and a single payout destination
- `p2p_fast_peer` only helps deliver early clean jobs; the final transaction set and fees still come from the next GBT template

## License

MIT
