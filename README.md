# amail-bridge

> Transparent bridge — one port, all agents; zero ports, email inbound.

A lightweight bridge between [amail relay](https://github.com/nousresearch/agent-mail-relay)
and [Hermes agent](https://github.com/nousresearch/hermes-agent) gateways.
Solves firewall penetration for multi-agent deployments and zero-dependency email
inbound.

---

## Why bridge

**Pain 1 — Multi-agent firewall penetration**: Each Hermes agent runs on its own
port (8645, 8646, …). Exposing them to the internet means N ports, N firewall
rules. Bridge's push mode provides a **single entry port** that auto-routes to
all agents — open just one port.

**Pain 2 — Zero-dependency email inbound**: No public IP? No port forwarding?
Pull mode uses a single **outbound HTTP long-poll** — bridge actively fetches
mail from relay and forwards to local gateways. Zero inbound ports required.

---

## Key features

| Feature | Description |
|---|---|
| **Transparent** | Bridge holds no HMAC secrets. Relay signs → bridge forwards verbatim → gateway verifies. Security boundary unchanged. |
| **Zero dependency** | Pure Rust, single binary ~3 MB. No Python, Node, or database required. |
| **Lightweight** | < 5 MB memory, near-zero CPU at idle. |
| **Aggregation** | One email, multiple recipients — relay sends a single body copy (push & pull). |
| **Zero config** | Auto-discovers agent routes from `~/.hermes/profiles/` via inotify. No manual route table. |
| **IP allowlist** | Optional push-mode allowlist — only relay IPs can POST, DDoS hardened. |
| **Graceful shutdown** | SIGINT / SIGTERM trigger graceful drain, in-flight requests complete. |
| **Daemon mode** | `--daemon` double-fork, systemd / Docker friendly. |
| **Auto TLS** | `tls = true` + `public_url` → automatic Let's Encrypt certificate via ACME HTTP-01. |

---

## Two modes

### Push — one port, all agents

```
                       ┌─────────────────────────────────┐
                       │         amail-bridge             │
                       │  (single public port 38080)       │
relay ──POST──►        │                                  │
  alice@...+bob@...    │  alice → 127.0.0.1:8645          │────► gateway:8645
  (one body copy)      │  bob   → 127.0.0.1:8646          │────► gateway:8646
                       │  carol → 127.0.0.1:8647          │────► gateway:8647
                       └─────────────────────────────────┘
```

- Relay POSTs to a **single port** on bridge; bridge auto-routes by agent email
- Multiple recipients → relay sends **one body copy** (batch aggregation)
- TLS via rustls; automatic Let's Encrypt certificate support

### Pull — zero ports, email inbound

```
relay (public)                               behind NAT/firewall
  │                                               │
  │◄── POST /pending (poll every 10s) ────────────│ bridge (outbound only)
  │                                               │
  │── batches [{body, deliveries}] ──────────────►│
  │                                               │
  │                                 ┌─────────────▼──────────────────┐
  │                                 │ fan-out to each gateway          │
  │                                 │ ACK forwarded deliveries         │
  │                                 └────────────────────────────────┘
  │◄── POST /pending/ack ─────────────────────────│
```

- Single **outbound HTTP connection** to relay, fully bypasses NAT/firewall
- Same batch aggregation: one body copy for all recipients
- ACK-based consumption + dedup cache — no messages lost, no duplicates

---

## Quickstart

```bash
git clone https://github.com/metercai/amail-bridge
cd amail-bridge
cargo build --release

# Push mode
cat > amail_bridge.toml << 'EOF'
mode = "push"
[push]
bind_port = 38080
public_url = "https://bridge.example.com"
allowed_ips = ["10.0.0.0/8"]
EOF

# Pull mode
cat > amail_bridge.toml << 'EOF'
mode = "pull"
[pull]
relay_url = "http://relay.example.com:38080"
admin_key = "sk-xxxxxxxx"
system_id = "admin"
EOF

# Run
./target/release/amail-bridge

# Or daemonize
./target/release/amail-bridge --daemon
```

---

## Configuration

### Push

```toml
mode = "push"

[push]
bind_host = "0.0.0.0"
bind_port = 38080
tls = false
public_url = "https://bridge.example.com"
allowed_ips = ["10.0.0.0/8", "172.16.0.1"]   # restrict to relay IPs (optional)

# Static TLS certificate (optional — ACME is preferred)
# tls_cert = "/etc/ssl/bridge.crt"
# tls_key  = "/etc/ssl/bridge.key"

# ACME auto-cert cache directory (optional, default: ~/.hermes/acme/)
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

### Multi-machine deployment

```toml
[hosts]
".*@admin.relay" = "192.168.1.2"   # all agents on this domain → this host
"alice@example.com" = "10.0.0.5"   # specific agent → specific host
```

### Environment variables

| Variable | Equivalent config |
|---|---|
| `AMAIL_BRIDGE_MODE` | `mode` |
| `AMAIL_BRIDGE_PUBLIC_URL` | `push.public_url` |
| `AMAIL_BRIDGE_RELAY_URL` | `pull.relay_url` |
| `AMAIL_BRIDGE_ADMIN_KEY` | `pull.admin_key` |
| `AMAIL_BRIDGE_SYSTEM_ID` | `pull.system_id` |
| `AMAIL_BRIDGE_POLL_SECS` | `pull.poll_interval_sec` |
| `AMAIL_BRIDGE_ALLOWED_IPS` | `push.allowed_ips` (comma-separated) |
| `HERMES_HOME` | Hermes home directory (default `~/.hermes`) |

---

## TLS & ACME

When `tls = true` and `public_url` is set, bridge automatically requests a
certificate from Let's Encrypt:

```
Startup
  ├─ tls_cert + tls_key present → use static certs (existing behavior)
  ├─ public_url set → extract domain, run ACME HTTP-01 challenge
  │   ├─ success → persist cert + key, start renew loop
  │   └─ failure → warn, fall back to plain HTTP
  └─ neither → warn, fall back to plain HTTP
```

- Domain is auto-extracted from `public_url` (`https://bridge.example.com` → `bridge.example.com`)
- Certificates are stored in `acme_cache` (default `~/.hermes/acme/`)
- Certificates auto-renew ~60 days after issuance (checks every 12 hours)
- ACME is compiled in by default (requires OpenSSL at build time)
- Disable TLS at compile time: `cargo build --no-default-features`

---

## Deployment

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

> Port 80 is needed for ACME HTTP-01 challenges. Omit if using static certs.

---

## Network scenarios

| Scenario | Mode | Notes |
|---|---|---|
| relay + gateway on same machine | Push | Bridge proxies single port to local gateways |
| relay public, gateway behind NAT | Pull | Bridge polls relay outbound, no inbound ports |
| Bridge on public VPS | Push + TLS | `tls=true`, `public_url=https://...`, ACME auto-cert |
| Multi-machine LAN | Push/Pull | `[hosts]` maps agent emails to machine IPs |

---

## Troubleshooting

| Symptom | Check |
|---|---|
| No routes | Profile directory has `amail.json` + `config.yaml`? |
| Pull: no deliveries | `admin_key` scope correct? `system_id` matches? |
| Push: 502 | Gateway webhook port listening? |
| Routes stale | `RUST_LOG=debug` to see inotify events |
| ACME: fallback to HTTP | Domain resolves to bridge? Port 80 reachable from internet? Check `RUST_LOG=info` for ACME errors |
