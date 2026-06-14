# amail-bridge

[🇨🇳 中文](README-zh.md)

![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange)
![Platform](https://img.shields.io/badge/platform-linux%20%7C%20macOS%20%7C%20windows-969696)
![Auto-Config](https://img.shields.io/badge/auto--config-inotify%20%7C%20ACME%20%7C%20routes-8A2BE2)
![License](https://img.shields.io/badge/License-GPL--3.0-blue)
![Tests](https://img.shields.io/badge/tests-17%20passing-brightgreen)
![Security](https://img.shields.io/badge/Security-OpenSSL%200-brightgreen)
![TLS](https://img.shields.io/badge/TLS-rustls-purple)
> Zero ports, email inbound. One port, instant forwarding to all agents.

A high-performance transparent bridge between [amail-gateway](https://github.com/metercai/amail-gateway)
and [Hermes agent](https://github.com/nousresearch/hermes-agent) gateway webhook endpoints.
Solves firewall penetration for heterogeneous multi-agent deployments with minimal
surface area.

---

## Why bridge

**Pain 1 — Multi-agent firewall penetration**: Each Hermes agent's webhook
runs on its own port (8645, 8646, …). Exposing them directly means N ports, N firewall
rules. Bridge's push mode provides a **single entry port** with auto-routing to every
webhook — open just one port, all agents instantly reachable.

**Pain 2 — Zero-dependency email inbound**: No public IP? No port forwarding? Pull mode
uses a single **outbound HTTP long-poll** — bridge actively fetches deliveries from
the gateway and fans out to local webhook ports. **Zero inbound ports, zero
listen sockets**, complete NAT/firewall bypass.

---

## Key features

### Secure transparent pass-through

Bridge holds zero HMAC secrets. Gateway signs with each agent's webhook secret →
bridge forwards headers & body verbatim → agent verifies. Security boundary
unchanged. Push mode supports IP allowlist + blacklist + per-IP rate limiting;
pull mode uses ACK-based consumption with 2-hour dedup cache — zero message loss,
zero duplicates.

### Lightweight, pure Rust, zero OpenSSL

Single binary ~8 MB (stripped, fat LTO). < 10 MB memory at idle, near-zero CPU.
Pure Rust TLS stack — rustls with ring crypto. Zero OpenSSL, zero native-tls,
zero system dependency beyond libc. `--daemon` double-fork daemon mode with
PID file and log file. SIGINT/SIGTERM graceful drain.

### Efficient aggregated forwarding

When one email reaches multiple recipients behind the same bridge, the gateway
sends a **single body copy** with per-recipient headers — bridge fans out to
each webhook port. Batch body serialized once, reused across all entries.
Works for both push and pull modes.

### Multi-machine bridge routing

A single bridge can route emails to agents on multiple machines.
Local agent profiles (`~/.hermes/profiles/*/`) are auto-discovered;
entries in `amail_routes.toml` (`"email" = "host:port"`) override them.
inotify hot-reloads on changes.

```toml
"alice@admin.relay" = "127.0.0.1:8645"
".*@admin\\.relay" = "192.168.1.2:8645"
```

### Security hardening

- **IP allowlist + blacklist** — push mode accepts POSTs only from trusted source IPs
- **Per-IP rate limiting** — configurable req/sec cap with sliding window (default 30)
- **Body size limit** — configurable cap (default 20 MB) prevents memory exhaustion
- **Header filtering** — only business headers forwarded (`x-amail-email`,
  `x-webhook-signature`, `x-mailrelay-timestamp`, `content-type`)
- **Graceful shutdown** — SIGINT/SIGTERM drain in-flight requests
- **Connection pooling** — reqwest client reused across all forwards (keep-alive)
- **HSTS on TLS only** — no HSTS header on plain HTTP (RFC 6797 compliance)

### Zero-config automation

- **Zero-config routing** — auto-scans `~/.hermes/profiles/` for agent webhook ports
- **inotify hot-reload** — detects profile changes, rescans immediately
- **ACME auto-TLS** — set `hostname` → automatic Let's Encrypt certificate
  (HTTP-01 challenge), cached and auto-renewed every ~60 days
- **Dual-port mode** — `addr` port 80 + `hostname` set → auto 80→443 redirect
- **Daemon mode** — `--daemon` double-fork, PID file, log file, zero supervision

---

## Two modes

### Push — one port, instant forwarding to all agents

```
                       ┌─────────────────────────────────┐
                       │         amail-bridge             │
                       │  (single public port 38080)       │
gateway ──POST──►      │                                  │
  alice@...+bob@...    │  alice → 127.0.0.1:8645          │──► webhook:8645
  (one body copy)      │  bob   → 127.0.0.1:8646          │──► webhook:8646
                       │  carol → 127.0.0.1:8647          │──► webhook:8647
                       └─────────────────────────────────┘
```

- Gateway POSTs to a **single port** on bridge; bridge auto-routes by agent email
- Multiple recipients → gateway sends **one body copy** (batch aggregation)
- TLS via rustls; automatic Let's Encrypt certificate when `hostname` is set
- Dual-port mode: `addr = "0.0.0.0:80"` + `hostname = "bridge.example.com"` → auto 80→443
- Real-time: gateway gets immediate HTTP response from agent via bridge

### Pull — zero ports, email inbound through NAT

```
gateway (public)                              behind NAT/firewall
  │                                               │
  │◄── POST /pending (poll every 10s) ────────────│ bridge (outbound only)
  │                                               │
  │── batches [{body, deliveries}] ──────────────►│
  │                                               │
  │                                 ┌─────────────▼──────────────────────┐
  │                                 │ fan-out to each agent webhook       │
  │                                 │ ACK forwarded deliveries            │
  │                                 └────────────────────────────────────┘
  │◄── POST /pending/ack ─────────────────────────│
```

- Single **outbound HTTP connection** to gateway, fully bypasses NAT/firewall
- **Zero listen sockets** — no ports opened, no inbound traffic at all
- Same batch aggregation: one body copy serialized once, reused for all recipients
- ACK-based consumption + 2-hour dedup cache — no messages lost, no duplicates
- Exponential backoff on fetch failures (max 5 minutes)

---

## Quickstart

```bash
git clone https://github.com/metercai/amail-bridge
cd amail-bridge
cargo build --release

# Push mode (single port, all agents)
cat > amail_bridge.toml << 'EOF'
mode = "push"
[push]
addr = "0.0.0.0:38080"
hostname = "bridge.example.com"     # enables TLS + ACME auto-cert
allowed_ips = ["10.0.0.0/8"]
EOF

# Pull mode (zero ports, outbound only)
cat > amail_bridge.toml << 'EOF'
mode = "pull"
[pull]
amail_url = "http://gateway.example.com:38080"
admin_key = "sk-xxxxxxxx"
system_id = "admin"
EOF

# Run
./target/release/amail-bridge

# Or daemonize
./target/release/amail-bridge --daemon

# Check health
curl http://localhost:38080/health
# {"status":"ok","uptime_secs":42,"version":"0.3.0"}
```

---

## Configuration

### Push

```toml
mode = "push"

[push]
addr = "0.0.0.0:38080"                # listen address (default: "0.0.0.0:38080")
hostname = "bridge.example.com"       # enables TLS + ACME auto-cert
# tls_cert = "/etc/ssl/bridge.crt"   # static TLS cert (optional)
# tls_key  = "/etc/ssl/bridge.key"   # static TLS key (optional)
# acme_cache = "./acme_cache"        # ACME cache dir (default: ./acme_cache)
blacklist_ips = ["1.2.3.4"]          # permanently blocked IPs (default: [])
allowed_ips = ["10.0.0.0/8"]         # IP allowlist, empty = allow all (default: [])
rate_limit = 30                       # req/sec per source IP, 0 = disabled (default: 30)
body_limit_mb = 20                    # max request body in MB (default: 20)
```

### Pull

```toml
mode = "pull"

[pull]
amail_url = "http://gateway.example.com:38080"
admin_key = "sk-xxxxxxxx"            # system admin API key from gateway
system_id = "admin"                  # system ID for pending query (default: "admin")
poll_interval_sec = 10               # poll interval in seconds (default: 10)
```

### Logging

```toml
[logging]
level = "info"                        # log level (default: "info")
file = "/var/log/amail-bridge.log"   # log file, stdout if unset (default: none)
```

### Multi-machine deployment

```toml
[hosts]
".*@admin.com" = "192.168.1.2"      # all agents on this domain → this host
"alice@example.com" = "10.0.0.5"    # specific agent → specific host
```

### Environment variables

| Variable | Equivalent config |
|---|---|
| `AMAIL_BRIDGE_MODE` | `mode` |
| `AMAIL_BRIDGE_HOSTNAME` | `push.hostname` |
| `AMAIL_GATEWAY_URL` | `pull.amail_url` |
| `AMAIL_BRIDGE_ADMIN_KEY` | `pull.admin_key` |
| `AMAIL_BRIDGE_SYSTEM_ID` | `pull.system_id` |
| `AMAIL_BRIDGE_POLL_SECS` | `pull.poll_interval_sec` |
| `AMAIL_BRIDGE_ALLOWED_IPS` | `push.allowed_ips` (comma-separated) |
| `HERMES_HOME` | Hermes home directory (default `~/.hermes`) |
| `RUST_LOG` | tracing filter (overrides `logging.level`) |

---

## TLS & ACME

When `hostname` is set, TLS is automatically enabled. Bridge attempts certificate
acquisition in this priority order:

```
Startup
  ├─ tls_cert + tls_key present → use static certs
  ├─ hostname set → run ACME HTTP-01 challenge (Let's Encrypt)
  │   ├─ success → persist cert + key to acme_cache, start auto-renew loop
  │   └─ failure → warn, fall back to plain HTTP (service continues)
  └─ no hostname → plain HTTP
```

- Certificate stored in `acme_cache` (default `./acme_cache`)
- Auto-renewed ~60 days after issuance (checks every 12 hours)
- **Requires port 80** to be available for HTTP-01 challenge validation
  (root or `CAP_NET_BIND_SERVICE` on Linux)
- Domain in `hostname` must resolve to the bridge server's public IP
- Pure Rust — zero OpenSSL dependency (instant-acme + ring crypto)

### Dual-port mode

When `addr` port is 80 and `hostname` is set:
- Port 80 serves ACME challenge validation + redirects to 443
- Port 443 serves the actual HTTPS application
- Single config line enables both — no manual TLS wiring needed

---

## Network scenarios

| Scenario | Mode | Notes |
|---|---|---|
| gateway + agents on same machine | Push | Bridge proxies single port to local webhook ports |
| gateway public, agents behind NAT | Pull | Bridge polls gateway outbound, zero inbound ports |
| Bridge on public VPS | Push + TLS | `hostname = "bridge.example.com"`, ACME auto-cert, dual-port |
| Multi-machine LAN | Push/Pull | `[hosts]` maps agent emails to machine IPs |

---

## Troubleshooting

| Symptom | Check |
|---|---|
| No routes | Profile directory has `amail.json` + `config.yaml`? |
| Pull: no deliveries | `admin_key` scope correct? `system_id` matches? |
| Push: 502 | Agent webhook port listening? |
| Routes stale | `RUST_LOG=debug` to see inotify events |
| ACME: fallback to HTTP | Domain resolves to bridge IP? Port 80 reachable? `RUST_LOG=debug` for ACME details |
| Port 80 busy | Free port 80 or use static certs, or set `addr` port ≠ 80 |
