# amail-bridge

> 透明桥接 — 一个端口，所有 agent；零端口，邮件入站。

[amail relay](https://github.com/nousresearch/agent-mail-relay) 和 [Hermes agent](https://github.com/nousresearch/hermes-agent) gateway webhook 端点之间的轻量桥接服务。
解决多 agent 部署时防火墙穿透和零依赖入站两大痛点。

---

## 为什么需要 bridge

**痛点 1 — 多 agent 防火墙穿透**：每个 Hermes agent 的 gateway webhook 跑在各自的端口上
（8645, 8646, …），部署到公网意味着要暴露 N 个端口、配 N 条防火墙规则。bridge 的 push 模式
提供一个**单一入口端口**，自动路由到所有 gateway webhook 端口，防火墙只需开一个端口。

**痛点 2 — 零依赖邮件入站**：没有公网 IP？没有端口映射？pull 模式只需一条**出站 HTTP 长轮询**，
由 bridge 主动从 relay 拉邮件并转发到本地 gateway webhook 端口，不需要任何入站端口。

---

## 核心特性

### 安全的透明透传

bridge 不持有、不验签 HMAC secret。relay 用各 agent 的 webhook secret 签名 →
bridge 原样转发 headers 和 body → gateway 验签。安全边界不变。push 模式支持
IP 白名单；pull 模式使用 ACK 消费 + 去重缓存，杜绝消息丢失和重复投递。

### 轻量零依赖进程

纯 Rust 单二进制，约 3 MB。内存 < 5 MB，CPU 近乎为零。不依赖 Python/Node/数据库。
`--daemon` 双 fork 守护进程，systemd/Docker 原生支持。SIGINT/SIGTERM 优雅排空。

### 高效的聚合转发

同一封邮件多个收件人在同一 bridge 后面时，relay→bridge 只传 **一份 body** + 每人
各自的 headers，bridge 再 fan-out 到各 gateway webhook 端口。推拉模式均支持。

### 正则匹配的多机转发

`[hosts]` 表以正则匹配 agent 邮箱 → 主机 IP，首匹配即胜，未匹配默认 `127.0.0.1`。
从 Hermes profiles 自动发现，可手动 routes TOML 文件覆盖。

### 多维度的安全加固

- **IP 白名单** — push 模式只接受受信 relay IP 的 POST
- **Body 大小限制** — 10 MB 上限防内存耗尽
- **优雅关闭** — SIGINT/SIGTERM 排空进行中请求后退出
- **Poison 锁恢复** — 所有锁路径 `unwrap_or_else` 恢复
- **Header 过滤** — 只转发业务 header（x-amail-email / x-webhook-signature / x-mailrelay-timestamp / content-type）
- **TLS** — rustls HTTPS，可选 Let's Encrypt ACME 自动证书

### 多项自动化配置

- **零配置路由** — 自动扫描 `~/.hermes/profiles/` 发现 agent webhook 端口，inotify 热更新
- **ACME 自动 TLS** — `tls = true` + `public_url` → 自动向 Let's Encrypt 申请证书
  （HTTP-01 挑战），缓存复用，自动续期
- **批量聚合** — relay 自动按 bridge URL 和 payload hash 分组，最小化 HTTP 往返次数和带宽
- **守护进程** — `--daemon` 双 fork，PID 文件、日志文件，无需人工看管

---

## 两种模式

### Push — 一个端口，所有 agent

```
                       ┌─────────────────────────────────┐
                       │         amail-bridge             │
                       │  (单一公网端口 38080)              │
relay ──POST──►        │                                  │
  alice@...+bob@...    │  alice → 127.0.0.1:8645          │────► gateway webhook:8645
  (同一份 body)         │  bob   → 127.0.0.1:8646          │────► gateway webhook:8646
                       │  carol → 127.0.0.1:8647          │────► gateway webhook:8647
                       └─────────────────────────────────┘
```

- relay 发到 bridge 的**单一端口**，bridge 按 agent 邮箱自动路由到对应 gateway webhook 端口
- 同一封邮件多个收件人时，relay→bridge 只传 **1 份 body**（批量聚合）
- 支持 TLS（rustls），可选自动 Let's Encrypt 证书

### Pull — 零端口，邮件入站

```
relay (公网)                              NAT/防火墙内
  │                                          │
  │◄── POST /pending (poll 每 10s) ──────────│ bridge (出站，无需开放端口)
  │                                          │
  │── batches [{body, deliveries}] ─────────►│
  │                                          │
  │                            ┌─────────────▼──────────────────┐
  │                            │ fan-out 到各 gateway webhook     │
  │                            │ ACK 已转发的 delivery            │
  │                            └────────────────────────────────┘
  │◄── POST /pending/ack ───────────────────│
```

- 只需要**一条出站 HTTP 连接**到 relay，完全穿透 NAT/防火墙
- 拉模式同样支持**批量聚合**：同一封邮件的 body 只传一份
- ACK 消费 + 2 小时去重缓存，不会丢消息也不会重复投递

---

## 快速开始

```bash
git clone https://github.com/metercai/amail-bridge
cd amail-bridge
cargo build --release

# Push 模式
cat > amail_bridge.toml << 'EOF'
mode = "push"
[push]
bind_port = 38080
public_url = "https://bridge.example.com"
allowed_ips = ["10.0.0.0/8"]
EOF

# Pull 模式
cat > amail_bridge.toml << 'EOF'
mode = "pull"
[pull]
relay_url = "http://relay.example.com:38080"
admin_key = "sk-xxxxxxxx"
system_id = "admin"
EOF

# 运行
./target/release/amail-bridge

# 或后台守护
./target/release/amail-bridge --daemon
```

---

## 配置参考

### Push

```toml
mode = "push"

[push]
bind_host = "0.0.0.0"
bind_port = 38080
tls = false
public_url = "https://bridge.example.com"
allowed_ips = ["10.0.0.0/8", "172.16.0.1"]   # 仅允许 relay 的 IP（可选）

# 静态 TLS 证书（可选 — 推荐用 ACME 自动申请）
# tls_cert = "/etc/ssl/bridge.crt"
# tls_key  = "/etc/ssl/bridge.key"

# ACME 自动证书缓存目录（可选，默认 ~/.hermes/acme/）
# acme_cache = "/var/lib/amail-bridge/acme"
```

### Pull

```toml
mode = "pull"

[pull]
relay_url = "http://relay.example.com:38080"
admin_key = "sk-xxxxxxxx"
system_id = "admin"
poll_interval_sec = 10
```

### 多机部署

```toml
[hosts]
".*@admin.relay" = "192.168.1.2"   # 域内所有 agent 路由到这台机器
"alice@example.com" = "10.0.0.5"   # 特定 agent 路由到指定 IP
```

### 环境变量

| 变量 | 对应配置 |
|---|---|
| `AMAIL_BRIDGE_MODE` | `mode` |
| `AMAIL_BRIDGE_PUBLIC_URL` | `push.public_url` |
| `AMAIL_BRIDGE_RELAY_URL` | `pull.relay_url` |
| `AMAIL_BRIDGE_ADMIN_KEY` | `pull.admin_key` |
| `AMAIL_BRIDGE_SYSTEM_ID` | `pull.system_id` |
| `AMAIL_BRIDGE_POLL_SECS` | `pull.poll_interval_sec` |
| `AMAIL_BRIDGE_ALLOWED_IPS` | `push.allowed_ips`（逗号分隔） |
| `HERMES_HOME` | Hermes 根目录（默认 `~/.hermes`） |

---

## TLS 与 ACME

当 `tls = true` 且 `public_url` 已设置时，bridge 自动通过 HTTP-01 挑战向
Let's Encrypt 申请证书：

```
启动流程
  ├─ tls_cert + tls_key 存在 → 使用静态证书（传统方式）
  ├─ public_url 已设置 → 提取域名，通过 ACME HTTP-01 挑战申请
  │   ├─ 成功 → 保存证书+密钥，启动后台续期
  │   └─ 失败 → 警告后回退到 HTTP（不影响服务）
  └─ 都没有 → 警告后回退到 HTTP
```

- 域名从 `public_url` 自动提取（`https://bridge.example.com` → `bridge.example.com`）
- 证书保存在 `acme_cache` 目录（默认 `~/.hermes/acme/`）
- 证书有效期 90 天，签发约 60 天后自动续期（每 12 小时检查一次）
- ACME 功能默认编译进二进制（编译时需要 OpenSSL 开发库）
- 不需要 TLS 时编译：`cargo build --no-default-features`

### 挑战方式

#### HTTP-01（已实现）

bridge 全自动处理，用户只需确保以下前提条件：

1. **DNS**：`public_url` 中的域名解析到 bridge 所在服务器的 IP
   ```bash
   dig bridge.example.com   # 必须返回 bridge 服务器的公网 IP
   ```
2. **防火墙**：bridge 服务器的 80 端口（TCP）对公网开放
3. **端口空闲**：80 端口没有被 nginx、Apache 等进程占用
   （bridge 仅在挑战期间临时绑定，完成后立即释放）

无需额外配置，只要 `tls = true` + `public_url` 即可。bridge 自动申请证书、
保存、续期。

#### DNS-01（尚未实现）

DNS-01 通过在 DNS 中创建 `_acme-challenge` TXT 记录来证明域名所有权，
不依赖 80 端口。实现后工作流程：

1. bridge 向 Let's Encrypt 发起挑战，获得 token
2. bridge 调用你的 DNS 服务商 API 创建记录：
   ```
   _acme-challenge.bridge.example.com   TXT   "<token>"
   ```
3. Let's Encrypt 查询 TXT 记录验证所有权
4. 证书签发，bridge 清理 TXT 记录

**预计需要配置**（未实现）：

```toml
[push]
tls = true
public_url = "https://bridge.example.com"
acme_challenge = "dns"            # "http"（默认）或 "dns"

[push.dns]
provider = "cloudflare"           # cloudflare / route53 / manual
api_token = "..."                 # 服务商 API 凭证
# zone_id = "..."                 # Route53 需要
```

对于没有 API 的 DNS 服务商，`manual` 模式会打印 TXT 记录值，等用户手动创建后回车继续：

```
$ amail-bridge
ACME DNS-01 挑战 — 请在 DNS 中添加以下 TXT 记录：
  _acme-challenge.bridge.example.com   TXT   "abc123def456"
创建完成后按回车继续...
```

**当前替代方案**：如果无法开放 80 端口，使用静态证书（`tls_cert` / `tls_key`），
或在 bridge 前放置 nginx / Caddy 处理 ACME。

---

## 部署

### systemd

```ini
[Unit]
Description=amail-bridge
After=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/amail-bridge
Restart=always
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

### Docker

```bash
docker run -d \
  -v ~/.hermes:/root/.hermes:ro \
  -p 38080:38080 \
  -p 80:80 \
  --name amail-bridge \
  ghcr.io/metercai/amail-bridge
```

> 端口 80 用于 ACME HTTP-01 挑战。使用静态证书可不映射。

---

## 网络场景

| 场景 | 模式 | 说明 |
|---|---|---|
| relay+gateway 同机 | Push | bridge 单端口转发到本地各 gateway webhook 端口 |
| relay 在公网，gateway 在 NAT 后 | Pull | bridge 出站轮询 relay，无需开放入站端口 |
| 公网 VPS 部署 bridge | Push + TLS | `tls=true`, `public_url=https://...`，ACME 自动证书 |
| 多机 LAN 部署 | Push/Pull | `[hosts]` 配置各 agent 所在机器 IP |

---

## 故障排查

| 现象 | 检查 |
|---|---|
| 无路由 | profile 目录是否有 `amail.json` + `config.yaml` |
| pull 无数据 | `admin_key` scope 正确？`system_id` 匹配？ |
| push 502 | gateway webhook 端口是否在监听 |
| 路由不更新 | `RUST_LOG=debug` 查看 inotify 事件 |
| ACME 回退到 HTTP | 域名是否解析到 bridge？80 端口公网可达？`RUST_LOG=info` 查看 ACME 错误 |
| 需要 DNS-01 | 改用静态证书或在 bridge 前放置反向代理 |
