# amail-bridge

> Transparent bridge between amail relay and Hermes gateway.

amail-bridge is a lightweight Rust service that sits between an
[amail relay](https://github.com/nousresearch/agent-mail-relay) and one or more
[Hermes agent](https://github.com/nousresearch/hermes-agent) gateway instances,
providing protocol bridge and network flexibility.

Two operating modes:

| Mode | Direction | Use when |
|------|-----------|----------|
| **Push** | relay → bridge → gateway | Bridge has a public IP / domain |
| **Pull** | bridge → relay → bridge → gateway | Bridge is behind NAT / firewall |

Both modes share the same zero-touch profile routing: bridge auto-discovers
gateway webhook ports from Hermes profile directories via inotify.

---

## Quickstart

```bash
# Build
git clone https://github.com/nousresearch/amail-bridge
cd amail-bridge
cargo build --release

# Configure — edit amail_bridge.toml
cp amail_bridge.toml ~/.hermes/
$EDITOR ~/.hermes/amail_bridge.toml

# Run
./target/release/amail-bridge
```

---

## Configuration

Bridge reads `amail_bridge.toml` from the current working directory,
with environment variable overrides.

### Push mode

```toml
mode = "push"

[push]
bind_host = "0.0.0.0"
bind_port = 38080
tls = false                          # Set true for HTTPS
public_url = "https://bridge.example.com"  # Printed at startup — admin copies to relay config

# Static certificate (optional)
# tls_cert = "/etc/ssl/bridge.crt"
# tls_key  = "/etc/ssl/bridge.key"

# Let's Encrypt ACME (optional, overrides static certs)
# acme_domain = "bridge.example.com"
# acme_cache = "~/.hermes/acme"
```

### Pull mode

```toml
mode = "pull"

[pull]
relay_url = "http://relay.example.com:38080"
admin_key = "sk-xxxxxxxx"            # system_admin API key from relay
system_id = "admin"
poll_interval_sec = 10               # Long-poll interval
```

### Environment variable overrides

| Variable | Equivalent config field |
|----------|------------------------|
| `AMAIL_BRIDGE_MODE` | `mode` |
| `AMAIL_BRIDGE_PUBLIC_URL` | `push.public_url` |
| `AMAIL_BRIDGE_RELAY_URL` | `pull.relay_url` |
| `AMAIL_BRIDGE_ADMIN_KEY` | `pull.admin_key` |
| `AMAIL_BRIDGE_SYSTEM_ID` | `pull.system_id` |
| `AMAIL_BRIDGE_POLL_SECS` | `pull.poll_interval_sec` |
| `HERMES_HOME` | Root Hermes directory (default: `~/.hermes`) |

---

## How it works

### Profile routing (auto-discovery)

Bridge scans `~/.hermes/profiles/*/` + `~/.hermes/` (default profile) for:

| Source file | Field | Used for |
|-------------|-------|----------|
| `amail.json` | `email` | Route key |
| `config.yaml` | `.platforms.webhook.extra.port` | Forward target port |

Builds a route table: `alice@admin.relay → port 8645`.

Uses inotify (via the `notify` crate) to watch for profile changes — new,
modified, or deleted profiles are picked up automatically without restart.

### Push mode

```
relay                                                     gateway
  │                                                         │
  │  POST https://bridge.example.com/webhooks/amail-inbound  │
  │  X-Amail-Email: alice@admin.relay                        │
  │  X-Webhook-Signature: sha256=...                         │
  │  {payload}                                               │
  ▼                                                         │
bridge ──lookup alice@admin.relay──→ port 8645               │
  │                                                         │
  │  POST 127.0.0.1:8645/webhooks/amail-inbound (verbatim)  │
  │  same headers + body                                    │
  └─────────────────────────────────────────────────────────►│
```

Bridge is a **transparent proxy**:
- Does NOT hold webhook secrets
- Does NOT verify or re-sign HMAC
- Relay signs with domain secret, gateway verifies with same secret
- Bridge just routes and forwards

After startup, bridge prints the `bridge_url` to copy into relay's
`~/.hermes/amail_relay.json`.

### Pull mode

```
relay                           NAT/firewall                    gateway
  │                                │                              │
  │◄── GET /pending?system_id=X ───│── bridge (outbound poll)     │
  │                                │                              │
  │── [{id:1, email, payload}] ──►│                              │
  │                                │                              │
  │        ┌───────────────────────▼──────────────────────────┐   │
  │        │ per-message: lookup email → port                  │   │
  │        │ POST 127.0.0.1:{port}/webhooks/amail-inbound      │──►│
  │        │ 2xx → ACK id                                      │   │
  │        └───────────────────────────────────────────────────┘   │
  │                                │                              │
  │◄── POST /pending/ack {ids} ────│                              │
  │                                │  sleep 10s → repeat          │
```

Key properties:
- **Single outbound connection** — works behind NAT/firewall
- **ACK-based consumption** — GET is non-destructive, POST /ack confirms
- **Crash-safe** — un-ACKed deliveries remain pending, re-delivered next poll
- **Dedup cache** — 2-hour TTL HashMap prevents double-forward on bridge restart
- **Unrouteable cleanup** — if an email has no matching profile route, it's ACKed
  to prevent accumulation

---

## Relay-side setup

### Step 1: Set delivery_mode

For pull mode, set `delivery_mode = "pull"` on the domain:

```bash
curl -X PUT http://relay:38080/api/v1/admin/system-domains/$DOMAIN_ID \
  -H "X-Api-Key: $ADMIN_KEY" \
  -H "Content-Type: application/json" \
  -d '{"delivery_mode": "pull"}'
```

For push mode (webhook — default), set `bridge_url` in relay config:

```json
// ~/.hermes/amail_relay.json
{
  "bridge_url": "https://bridge.example.com/webhooks/amail-inbound"
}
```

### Step 2: Hermes integration

When creating a Hermes profile, the amail integration (`amail_tools.py`)
auto-registers the agent's email with the relay. If `bridge_url` is set in
relay config, it's used as the webhook URL instead of `webhook_host:port`.

```bash
# Environment variable (optional)
export AMAIL_BRIDGE_URL="https://bridge.example.com/webhooks/amail-inbound"
```

---

## Deployment

### systemd

```ini
# /etc/systemd/system/amail-bridge.service
[Unit]
Description=amail-bridge — relay-gateway bridge
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=ubuntu
WorkingDirectory=/home/ubuntu/.hermes
ExecStart=/usr/local/bin/amail-bridge
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

```bash
sudo cp target/release/amail-bridge /usr/local/bin/
sudo systemctl daemon-reload
sudo systemctl enable --now amail-bridge
```

### Docker

```dockerfile
FROM rust:1.80-slim-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/amail-bridge /usr/local/bin/
COPY amail_bridge.toml /etc/amail-bridge.toml
WORKDIR /etc
ENTRYPOINT ["/usr/local/bin/amail-bridge"]
```

```bash
docker build -t amail-bridge .
docker run -d \
  -v ~/.hermes:/root/.hermes:ro \
  -v /etc/amail-bridge.toml:/etc/amail_bridge.toml:ro \
  -p 38080:38080 \
  --name amail-bridge \
  amail-bridge
```

---

## Network scenarios

| Scenario | Mode | Configuration |
|----------|------|---------------|
| Bridge on same machine as relay+gateway | Push or Pull | Use direct 127.0.0.1 URLs |
| Bridge on LAN, relay on LAN | Push | `bind_host=0.0.0.0`, relay uses LAN IP |
| Bridge on public VPS, relay on VPS | Push + HTTPS | `tls=true`, `public_url=https://...` |
| Bridge behind NAT, relay on internet | Pull | Single outbound poll to relay |
| Docker bridge + relay on host | Push or Pull | Use `host.docker.internal` or `--network=host` |

---

## Troubleshooting

| Symptom | Check |
|---------|-------|
| No routes loaded | `amail.json` + `config.yaml` exist in profile dirs? |
| Pull: no deliveries fetched | `admin_key` has `system_admin` scope? `system_id` correct? |
| Push: 502 Bad Gateway | Gateway webhook port running? Profile has `platforms.webhook.extra.port`? |
| Pull: stale pending > 24h | Relay logs warn — bridge may be disconnected |
| Routes stale after profile change | Check `RUST_LOG=debug` for inotify events |
