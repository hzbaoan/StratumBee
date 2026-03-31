# StratumBee

StratumBee 是一个使用 Rust 编写的轻量级 Solo Bitcoin Stratum V1 网关，面向 VPS、树莓派和其他低资源主机。

它把 `bitcoind` 的 `getblocktemplate`、longpoll、可选 ZMQ，以及可选的 Bitcoin P2P 快速区块通告接到 Stratum V1 矿机侧，并在本地完成 share 校验、整块组装、候选区块归档、`submitblock` 提交和只读监控展示。

## 项目定位

StratumBee 适合下面这类场景：

- 你已经有一台 Bitcoin Core 节点，想给矿机提供一个轻量 solo 入口
- 你不需要矿池账户系统、数据库、支付结算或用户余额管理
- 你希望 share 校验和区块提交逻辑尽量在本地完成，而不是依赖上游矿池
- 你希望在低内存环境中运行，并通过只读页面查看在线矿机和区块状态

StratumBee 不做这些事：

- 不提供矿池分账、收益分配、余额结算
- 不根据矿工用户名动态切换 payout 地址
- 不提供写入型管理 API
- 不实现 Stratum V2

## 当前能力

- 基于 `getblocktemplate` 拉取模板，使用 longpoll 作为主更新通道
- 支持订阅 `hashblock` / `rawblock` ZMQ 事件，加快新区块后的模板刷新
- 支持配置 `bitcoin.p2p_fast_peer`，通过 `headers` / `cmpctblock` / `inv` 更快发出新区块 clean job
- 提供 Stratum V1 服务，支持 `subscribe`、`authorize`、`submit`、`suggest_difficulty`、`configure`、`get_version`、`ping`
- 支持 version rolling 协商，允许掩码范围为 `1fffe000`
- 本地精确校验 share，计算真实 share difficulty 和是否命中全网目标
- 命中区块时在本地组装完整 `block hex`，按重试策略调用 `submitblock`
- 把 `submitblock` 返回的 `duplicate` 视为成功，避免误报丢块
- 可选在提交前把候选区块归档到本地目录
- 内存中保留矿工状态、share 事件和最近区块记录，无需额外数据库
- 自带只读 dashboard 和 JSON API
- 支持 Vardiff，按目标 share 时间自动上下调整难度

## 工作方式

1. 启动时从 Bitcoin Core 拉取第一个 `getblocktemplate` 模板。
2. 之后主要通过 longpoll 等待模板变化；如果配置了 ZMQ，收到 `hashblock` / `rawblock` 后也会主动刷新。
3. 如果配置了 `bitcoin.p2p_fast_peer`，收到 `headers` / `cmpctblock` 等新区块通告时，会先构造一个仅含 coinbase 的快速 clean job，尽早让矿机切到新区块。
4. 完整交易列表、手续费和 witness commitment 仍以后续 GBT / longpoll 返回的真实模板为准。
5. 矿机提交 share 后，StratumBee 会在本地重建 coinbase、merkle root 和 block header，并校验 share target / block target。
6. 如果命中区块目标，会归档候选区块、提交到 Bitcoin Core，并把状态记录为 `candidate`、`submitted`、`duplicate`、`rejected` 或 `submit_failed`。

## 运行要求

### 软件要求

- Rust `1.85` 或更高版本
- Bitcoin Core 节点，并开启 RPC
- 如果启用 ZMQ，需要在 Bitcoin Core 打开 `hashblock` / `rawblock` 发布
- 如果启用 `p2p_fast_peer`，需要 StratumBee 能连接到目标节点的 Bitcoin P2P 端口
- Linux 下编译需要 ZeroMQ 开发库

### Debian / Ubuntu 依赖

```bash
apt-get update
apt-get install -y build-essential pkg-config libzmq3-dev
```

## Bitcoin Core 配置

至少需要开启 RPC。为了更快感知新区块，建议同时开启 ZMQ：

```ini
server=1
rpcuser=bitcoinrpc
rpcpassword=change-me
rpcallowip=127.0.0.1

zmqpubhashblock=tcp://127.0.0.1:28334
zmqpubrawblock=tcp://127.0.0.1:28332
```

说明：

- `zmqpubhashblock` 和 `zmqpubrawblock` 是可选的，但推荐开启
- 如果不配置 ZMQ，StratumBee 仅依赖首次 GBT 拉取和 longpoll
- 如果配置 ZMQ，则使用 `longpoll + ZMQ`
- `p2p_fast_peer` 不替代 GBT 刷新，它只负责更早地发出新区块 clean job
- 如需启用 `p2p_fast_peer`，确保目标节点的 P2P 端口可达；如果只写主机名不写端口，会按网络自动使用默认端口（mainnet `8333`、testnet `18333`、signet `38333`、regtest `18444`）

## 快速开始

1. 编辑 [`config/stratumbee.toml`](config/stratumbee.toml)。
2. 设置 Bitcoin Core RPC 地址、用户名和密码。
3. 设置 `mining.payout_address`，或直接设置 `mining.payout_script_hex`。
4. 按需开启 `bitcoin.zmq_block_urls` 和 `bitcoin.p2p_fast_peer`。
5. 启动服务。

开发运行：

```bash
cargo run --release --locked -- --config config/stratumbee.toml
```

编译后运行：

```bash
cargo build --release --locked
./target/release/stratumbee --config config/stratumbee.toml
```

仓库已提交 [`Cargo.lock`](Cargo.lock)。建议本地构建、CI 和容器构建都使用 `--locked`，确保依赖版本可复现。

默认端口：

- Stratum: `3333`
- API / Dashboard: `8080`

## 配置说明

示例配置位于 [`config/stratumbee.toml`](config/stratumbee.toml)。

### `[bitcoin]`

| 配置项 | 说明 |
| --- | --- |
| `network` | `mainnet` / `testnet` / `signet` / `regtest` |
| `rpc_url` | Bitcoin Core RPC 地址 |
| `rpc_user` | RPC 用户名 |
| `rpc_pass` | RPC 密码 |
| `zmq_block_urls` | 可选，ZMQ 地址列表，可同时填多个 |
| `p2p_fast_peer` | 可选，快速区块通告来源节点，支持 `host` 或 `host:port` |

### `[stratum]`

| 配置项 | 说明 |
| --- | --- |
| `bind` / `port` | Stratum 监听地址与端口 |
| `extranonce1_size` | `extranonce1` 字节数，范围 `1..=16` |
| `extranonce2_size` | `extranonce2` 字节数，范围 `1..=16` |
| `min_difficulty` | 最小难度 |
| `max_difficulty` | 最大难度 |
| `start_difficulty` | 新连接初始难度，会被限制在最小/最大难度之间 |
| `target_share_time_secs` | Vardiff 目标 share 时间 |
| `vardiff_retarget_time_secs` | Vardiff 重算周期 |
| `vardiff_enabled` | 是否开启 Vardiff |
| `auth_token` | 可选密码；设置后矿机需要把该值放到 `mining.authorize` 的密码字段 |
| `max_line_bytes` | 单条 Stratum 报文最大长度 |
| `idle_timeout_secs` | 连接空闲超时 |

### `[api]`

| 配置项 | 说明 |
| --- | --- |
| `bind` / `port` | API 监听地址与端口 |
| `enabled` | 是否启用只读 dashboard 和 API |

### `[mining]`

| 配置项 | 说明 |
| --- | --- |
| `payout_address` | 挖到块后的固定收款地址 |
| `payout_script_hex` | 可选，直接指定输出脚本；设置后可不依赖地址解析 |
| `pool_tag` | coinbase 中的池标签，最长 32 字节 |
| `coinbase_message` | coinbase 附加消息，最长 32 字节 |
| `save_solved_blocks_dir` | 候选区块归档目录 |
| `block_archive_pre_submit` | 是否在 `submitblock` 前归档候选区块 |

### `[runtime]`

| 配置项 | 说明 |
| --- | --- |
| `notify_bucket_capacity` | 非 clean job 通知的令牌桶容量 |
| `notify_bucket_refill_ms` | 通知令牌恢复速度 |
| `miner_inactive_timeout_secs` | 多久未活动后从 dashboard 中移除矿工 |
| `max_recent_events` | 最近 share 事件缓存数量 |
| `max_block_history` | 最近区块历史数量 |

### 已移除的旧配置

以下字段已经移除，保留在配置文件中会直接报错：

- `runtime.template_poll_ms`
- `stratum.job_refresh_ms`

当前实现里，真实模板刷新只依赖首次 GBT、longpoll 和可选 ZMQ；`p2p_fast_peer` 只负责加速新区块切换，不负责生成完整 GBT 模板。

### 环境变量覆盖

以下字段支持环境变量覆盖：

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

最常见的用法：

```bash
export STRATUMBEE_RPC_URL=http://127.0.0.1:8332
export STRATUMBEE_RPC_USER=bitcoinrpc
export STRATUMBEE_RPC_PASS=change-me
export STRATUMBEE_PAYOUT_ADDRESS=bc1qreplace-with-your-address
```

## 矿机接入

矿机地址：

```text
stratum+tcp://YOUR_HOST:3333
```

接入规则：

- 收款地址始终来自 `mining.payout_address` 或 `mining.payout_script_hex`
- 矿机用户名不会改变 payout 地址
- 如果未设置 `stratum.auth_token`，`mining.authorize` 基本都会通过
- 如果设置了 `stratum.auth_token`，矿机需要把该 token 放在密码字段
- dashboard 中的矿工标识来自 `user_agent + session suffix`，不是比特币地址

## 支持的 Stratum 行为

当前实现支持：

- `mining.subscribe`
- `mining.extranonce.subscribe`
- `mining.authorize`
- `mining.submit`
- `mining.suggest_difficulty`
- `mining.configure`
- `mining.get_version`
- `mining.ping`

补充说明：

- `mining.configure` 支持 version rolling，并把协商掩码限制在 `1fffe000`
- share 去重基于 `job_id + nonce + ntime + extranonce2 + version`
- stale share 会区分为新区块切换、作业过期、短时间重连三类

## 监控页面和 API

如果 `api.enabled = true`，默认会在 `127.0.0.1:8080` 启动只读页面和 JSON API。

### 页面入口

- `/`：内置 dashboard
- `/health`：健康检查；当模板尚未准备好时返回 `503`

### JSON 接口

| 路径 | 说明 |
| --- | --- |
| `/api/stats` | 基础统计、模板状态、算力估算 |
| `/api/workers` | 简化矿工列表 |
| `/api/v1/summary` | 汇总指标、提交通道状态、最佳 share |
| `/api/v1/miners` | 完整矿工状态 |
| `/api/v1/blocks` | 最近候选/提交区块记录 |
| `/api/v1/events` | 最近 30 分钟 share 事件 |
| `/api/v1/network` | 通过 `getmininginfo` 获取的网络信息 |
| `/api/v1/template` | 当前模板详情 |
| `/api/v1/mempool` | 通过 `getmempoolinfo` 获取的 mempool 信息 |

说明：

- API 全部是只读的
- CORS 当前为全开放，直接暴露到公网前建议把 `api.bind` 保持为 `127.0.0.1`
- 如需外网访问，建议放到你自己的反向代理后面

## Docker

仓库提供了 [`Dockerfile`](Dockerfile)、[`.dockerignore`](.dockerignore)、[`docker-compose.yml`](docker-compose.yml)，以及用于构建并推送容器镜像的 [`.github/workflows/docker.yml`](.github/workflows/docker.yml)。

启动：

```bash
docker compose up -d --build
```

当前镜像与 Compose 配置特点：

- `Dockerfile` 使用多阶段构建：编译阶段基于 `rust:1.85-bookworm`，运行阶段基于 `debian:bookworm-slim`
- 构建阶段会复制 `Cargo.toml` 和 `Cargo.lock`，并执行 `cargo build --release --locked`
- [`.dockerignore`](.dockerignore) 会排除 `.git`、`.github`、`deploy`、`target`、`README.md` 和旧 `docker` 路径，减少构建上下文
- 运行镜像内置 `config/` 和 `assets/`
- 暴露 `3333` 和 `8080`
- Compose 使用 `network_mode: host`
- 挂载 `./config` 到容器内 `/opt/stratumbee/config`
- 挂载 `./var` 到容器内 `/opt/stratumbee/var`
- 默认通过环境变量注入 `RUST_LOG`、RPC 用户名和密码

镜像工作流说明：

- [`.github/workflows/docker.yml`](.github/workflows/docker.yml) 通过手动触发 `workflow_dispatch` 执行
- 工作流会将镜像推送到 `ghcr.io/<owner>/<repo>`
- 默认生成 `latest` 和提交 SHA 两类标签，目标平台为 `linux/amd64`

这套 Compose 更适合 Linux 主机；如果你运行在 Docker Desktop 上，需要自行调整网络暴露方式。

## systemd

可参考 [`deploy/systemd/stratumbee.service`](deploy/systemd/stratumbee.service)。

这个 service 模板默认：

- 工作目录为 `/opt/stratumbee`
- 从 `/opt/stratumbee/config/stratumbee.toml` 读取配置
- 可选读取环境变量文件 `/opt/stratumbee/config/stratumbee.env`
- 把 `/opt/stratumbee/var` 作为可写目录
- 以 `stratumbee:stratumbee` 用户运行

部署前请确认：

- 已创建 `stratumbee` 用户和组
- 二进制位于 `/usr/local/bin/stratumbee`
- 配置文件和 `var` 目录已准备好

## 候选区块归档

当矿机提交命中全网目标的 share 时，StratumBee 可以在提交前把候选区块写入本地 JSON 文件。

默认目录：

```text
var/solved-blocks
```

归档内容包含：

- 区块高度
- 区块哈希
- 模板标识
- `block_hex`
- `coinbase_hex`
- 保存时间

## 仓库结构

| 路径 | 说明 |
| --- | --- |
| `src/main.rs` | 入口，加载配置并启动模板、Stratum、API 和可选 P2P 快速通告 |
| `src/config.rs` | TOML 配置加载、环境变量覆盖、校验与旧字段拒绝 |
| `src/template.rs` | GBT、longpoll、ZMQ、快速空模板、coinbase 组装、`submitblock` |
| `src/stratum.rs` | Stratum V1 会话、notify、submit、version rolling、Vardiff |
| `src/share.rs` | 本地 share / block 校验与整块组装 |
| `src/metrics.rs` | 内存指标、事件和区块历史 |
| `src/api.rs` | Dashboard 与只读 JSON API |
| `src/p2p.rs` | Bitcoin P2P 快速区块通告客户端 |
| `src/rpc.rs` | Bitcoin Core RPC / longpoll 客户端 |
| `src/hash.rs` | 哈希与 merkle 相关辅助函数 |
| `src/vardiff.rs` | Vardiff 控制器 |
| `assets/dashboard.html` | 单文件监控页面 |
| `config/stratumbee.toml` | 示例配置 |
| `Cargo.lock` | 锁定 Rust 依赖版本，供 `--locked` 构建使用 |
| `.dockerignore` | 容器构建上下文排除规则 |
| `Dockerfile` | 容器镜像构建 |
| `docker-compose.yml` | 容器部署示例 |
| `.github/workflows/docker.yml` | 手动构建并推送 GHCR 镜像的 GitHub Actions 工作流 |
| `deploy/systemd/stratumbee.service` | systemd 模板 |

## 已知边界

- 指标和事件只保存在内存中，进程重启后不会保留历史
- 不提供 TLS 终止；如需公网访问，请放到反向代理后面
- 没有矿工账户体系，也没有 payout accounting
- 当前更偏向单节点、单 payout 地址的 solo 部署模型
- `p2p_fast_peer` 只会提前下发快速 clean job，真实交易集合和手续费仍以后续 GBT 模板为准

## 许可协议

MIT
