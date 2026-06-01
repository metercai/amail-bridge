# amail-bridge

> 透明桥接 — 一个端口，所有 agent；零端口，邮件入站。

[amail relay](https://github.com/nousresearch/agent-mail-relay) 和 [Hermes agent](https://github.com/nousresearch/hermes-agent) gateway 之间的轻量桥接服务。
解决多 agent 部署时防火墙穿透和零依赖入站两大痛点。

---

## 为什么需要 bridge

**痛点 1 — 多 agent 防火墙穿透**：每个 Hermes agent 跑在各自的端口上（8645, 8646, …），
部署到公网意味着要暴露 N 个端口、配 N 条防火墙规则。bridge 的 push 模式提供一个**单一
入口端口**，自动路由到所有 agent，防火墙只需开一个端口。

**痛点 2 — 零依赖邮件入站**：没有公网 IP？没有端口映射？pull 模式只需一条**出站 HTTP
长轮询**，由 bridge 主动从 relay 拉邮件并转发到本地 gateway，不需要任何入站端口。

---

## 核心特性

| 特性 | 说明 |
|---|---|
| **透传** | bridge 不持有、不验签 HMAC secret，relay 签名→bridge 原样转发→gateway 验签，安全边界不变 |
| **零依赖** | 纯 Rust 单二进制，2.5 MB，不依赖 Python/Node/数据库 |
| **轻量** | 内存占用 < 5 MB，CPU 近乎为零 |
| **聚合** | 同一封邮件多个收件人，relay→bridge 只传一份 body（推拉均支持） |
| **零配置** | 自动扫描 `~/.hermes/profiles/` 发现所有 agent 的路由，inotify 热更新，无需手动维护路由表 |
| **IP 白名单** | push 模式可选白名单，只允许 relay 的 IP 访问，防 DDoS |
| **优雅关闭** | SIGINT/SIGTERM 触发 graceful shutdown，正在处理的请求完成后再退出 |
| **daemon 模式** | `--daemon` 双 fork 守护进程，systemd/Docker 友好 |

---

## 两种模式

### Push — 一个端口，所有 agent

```
                       ┌─────────────────────────────────┐
                       │         amail-bridge             │
                       │  (单一公网端口 38080)              │
relay ──POST──►        │                                  │
  alice@...+bob@...    │  alice → 127.0.0.1:8645          │────► gateway:8645
  (同一份 body)         │  bob   → 127.0.0.1:8646          │────► gateway:8646
                       │  carol → 127.0.0.1:8647          │────► gateway:8647
                       └─────────────────────────────────┘
```

- relay 发到 bridge 的**单一端口**，bridge 按 agent 邮箱自动路由到对应 gateway
- 同一封邮件多个收件人时，relay→bridge 只传 **1 份 body**（批量聚合）
- 支持 TLS（rustls）

### Pull — 零端口，邮件入站

```
relay (公网)                              NAT/防火墙内
  │                                          │
  │◄── POST /pending (poll 每 10s) ──────────│ bridge (出站，无需开放端口)
  │                                          │
  │── batches [{body, deliveries}] ─────────►│
  │                                          │
  │                            ┌─────────────▼──────────────────┐
  │                            │ fan-out 到各 gateway             │
  │                            │ ACK 已转发的 delivery            │
  │                            └────────────────────────────────┘
  │◄── POST /pending/ack ───────────────────│
```

- 只需要**一条出站 HTTP 连接**到 relay，完全穿透 NAT/防火墙
- 拉模式同样支持**批量聚合**：同一封邮件的 body 只传一份
- ACK 消费 + 去重缓存，不会丢消息也不会重复投递

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
  --name amail-bridge \
  ghcr.io/metercai/amail-bridge
```

---

## 网络场景

| 场景 | 模式 | 说明 |
|---|---|---|
| relay+gateway 同机 | Push | bridge 单端口转发到本地各 gateway 端口 |
| relay 在公网，gateway 在 NAT 后 | Pull | bridge 出站轮询 relay，无需开放入站端口 |
| 公网 VPS 部署 bridge | Push + TLS | `tls=true`, `public_url=https://...` |
| 多机 LAN 部署 | Push/Pull | `[hosts]` 配置各 agent 所在机器 IP |

---

## 故障排查

| 现象 | 检查 |
|---|---|
| 无路由 | profile 目录是否有 `amail.json` + `config.yaml` |
| pull 无数据 | `admin_key` scope 正确？`system_id` 匹配？ |
| push 502 | gateway webhook 端口是否在监听 |
| 路由不更新 | `RUST_LOG=debug` 查看 inotify 事件 |
