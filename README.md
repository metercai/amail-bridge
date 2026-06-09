# amail-bridge

> Zero ports, email inbound. One port, instant forwarding to all agents.

A high-performance transparent bridge between [amail-gateway](https://github.com/metercai/amail-gateway)
and [Hermes agent](https://github.com/nousresearch/hermes-agent) gateway webhook endpoints.
Solves firewall penetration for heterogeneous multi-agent deployments with minimal
surface area.

---

## Why bridge

**Pain 1 — Multi-agent firewall penetration**: Each Hermes agent's gateway webhook
runs on its own port (8645, 8646, …). Exposing them directly means N ports, N firewall
rules. Bridge's push mode provides a **single entry port** with auto-routing to every
gateway webhook — open just one port, all agents instantly reachable.

**Pain 2 — Zero-dependency email inbound**: No public IP? No port forwarding? Pull mode
uses a single **outbound HTTP long-poll** — bridge actively fetches deliveries from
relay and fans out to local gateway webhook ports. **Zero inbound ports, zero
listen sockets**, complete NAT/firewall bypass.

---

## Key features

### Secure transparent pass-through

Bridge holds zero HMAC secrets. Relay signs with each agent's webhook secret →
bridge forwards headers & body verbatim → gateway verifies. Security boundary
unchanged. Push mode supports IP allowlist + blacklist + per-IP rate limiting;
pull mode uses ACK-based consumption with 2-hour dedup cache — zero message loss,
zero duplicates.

### Lightweight, pure Rust, zero OpenSSL

Single binary ~8 MB (stripped, fat LTO). < 10 MB memory at idle, near-zero CPU.
Pure Rust TLS stack — rustls with ring crypto. Zero OpenSSL, zero native-tls,
zero system dependency beyond libc. `--daemon` double-fork daemon mode for
systemd/Docker deployment. SIGINT/SIGTERM graceful drain with PID file cleanup.

### Efficient aggregated forwarding

When one email reaches multiple recipients behind the same bridge, the relay
sends a **single body copy** with per-recipient headers — bridge fans out to
each gateway webhook port. Batch body serialized once, reused across all entries.
Works for both push and pull modes.

### Regex-based multi-machine routing

`[hosts]` table maps agent email patterns (regex) to host IPs. First-match-wins,
unmatched agents default to `127.0.0.1`. Auto-discovered from Hermes profiles
(`~/.hermes/profiles/*/amail.json` + `config.yaml`), overridable via manual
`amail-bridge-routes.toml` file. inotify watcher hot-reloads on config changes.

### Security hardening

- **IP allowlist + blacklist** — push mode accepts POSTs only from trusted relay IPs
- **Per-IP rate limiting** — configurable req/sec cap with sliding window (default 30)
- **Body size limit** — configurable cap (default 20 MB) prevents memory exhaustion
- **Header filtering** — only business headers forwarded (`x-amail-email`,
  `x-webhook-signature`, `x-mailrelay-timestamp`, `content-type`)
- **Graceful shutdown** — SIGINT/SIGTERM drain in-flight requests, clean PID removal
- **Path traversal prevention** — vhost static file serving validates resolved paths
- **Connection pooling** — reqwest client reused across all forwards (keep-alive)
- **Proxy headers** — vhost reverse-proxy correctly sets `X-Forwarded-*` headers
- **HSTS on TLS only** — no HSTS header on plain HTTP (RFC 6797 compliance)

### Zero-config automation

- **Zero-config routing** — auto-scans `~/.hermes/profiles/` for agent webhook ports
- **inotify hot-reload** — detects profile changes, rescans immediately
- **ACME auto-TLS** — set `hostname` → automatic Let's Encrypt certificate
  (HTTP-01 challenge), cached and auto-renewed every ~60 days
- **Dual-port mode** — `addr` port 80 + `hostname` set → auto 80→443 redirect
- **Daemon mode** — `--daemon` double-fork, PID file, log file, zero supervision

### amail-gateway aligned

- Config format: unified `addr = "host:port"` (not split host/port fields)
- `hostname` → implies TLS (no redundant `tls = true` field)
- Logging: same `[logging]` section with `level` + `file`, `init_tracing()` pattern
- Health endpoint: `GET /health` returns `{"status":"ok","uptime_secs":N,"version":"x.y.z"}`
- Operation logs: `info!()` on all core paths — webhook relayed, pull cycle complete,
  routes scanned, ACME renewed

---

## Two modes

### Push — one port, instant forwarding to all agents

```
                       ┌─────────────────────────────────┐
                       │         amail-bridge             │
                       │  (single public port 38080)       │
relay ──POST──►        │                                  │
  alice@...+bob@...    │  alice → 127.0.0.1:8645          │──► gateway webhook:8645
  (one body copy)      │  bob   → 127.0.0.1:8646          │──► gateway webhook:8646
                       │  carol → 127.0.0.1:8647          │──► gateway webhook:8647
                       └─────────────────────────────────┘
```

- Relay POSTs to a **single port** on bridge; bridge auto-routes by agent email
- Multiple recipients → relay sends **one body copy** (batch aggregation)
- TLS via rustls; automatic Let's Encrypt certificate when `hostname` is set
- Dual-port mode: `addr = "0.0.0.0:80"` + `hostname = "bridge.example.com"` → auto 80→443
- Real-time: relay gets immediate HTTP response from gateway via bridge

### Pull — zero ports, email inbound through NAT

```
relay (public)                               behind NAT/firewall
  │                                               │
  │◄── POST /pending (poll every 10s) ────────────│ bridge (outbound only)
  │                                               │
  │── batches [{body, deliveries}] ──────────────►│
  │                                               │
  │                                 ┌─────────────▼──────────────────┐
  │                                 │ fan-out to each gateway webhook  │
  │                                 │ ACK forwarded deliveries         │
  │                                 └────────────────────────────────┘
  │◄── POST /pending/ack ─────────────────────────│
```

- Single **outbound HTTP connection** to relay, fully bypasses NAT/firewall
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
relay_url = "http://relay.example.com:38080"
admin_key = "sk-xxxxxxxx"
system_id = "admin"
EOF

# Run (foreground)
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

# Virtual host sites (optional)
# [[push.sites]]
# domain = "www.example.com"
# root = "/var/www/example"          # static site directory
#
# [[push.sites]]
# domain = "api.example.com"
# proxy = "127.0.0.1:3000"           # reverse proxy target
```

### Pull

```toml
mode = "pull"

[pull]
relay_url = "http://relay.example.com:38080"
admin_key = "sk-xxxxxxxx"            # system admin API key from relay
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
".*@admin.relay" = "192.168.1.2"    # all agents on this domain → this host
"alice@example.com" = "10.0.0.5"    # specific agent → specific host
```

### Environment variables

| Variable | Equivalent config |
|---|---|
| `AMAIL_BRIDGE_MODE` | `mode` |
| `AMAIL_BRIDGE_HOSTNAME` | `push.hostname` |
| `AMAIL_BRIDGE_RELAY_URL` | `pull.relay_url` |
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
> The binary has `CAP_NET_BIND_SERVICE` or runs as root for port 80 binding.

---

## Network scenarios

| Scenario | Mode | Notes |
|---|---|---|
| relay + gateway on same machine | Push | Bridge proxies single port to local gateway webhook ports |
| relay public, gateway behind NAT | Pull | Bridge polls relay outbound, zero inbound ports |
| Bridge on public VPS | Push + TLS | `hostname = "bridge.example.com"`, ACME auto-cert, dual-port |
| Multi-machine LAN | Push/Pull | `[hosts]` maps agent emails to machine IPs |

---

## Troubleshooting

| Symptom | Check |
|---|---|
| No routes | Profile directory has `amail.json` + `config.yaml`? |
| Pull: no deliveries | `admin_key` scope correct? `system_id` matches? |
| Push: 502 | Gateway webhook port listening? |
| Routes stale | `RUST_LOG=debug` to see inotify events |
| ACME: fallback to HTTP | Domain resolves to bridge IP? Port 80 reachable? `RUST_LOG=debug` for ACME details |
| Port 80 busy | Free port 80 or use static certs, or set `addr` port ≠ 80 |
