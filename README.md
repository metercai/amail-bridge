# amail-bridge

> Transparent bridge — one port, all agents; zero ports, email inbound.

A lightweight bridge between [amail relay](https://github.com/nousresearch/agent-mail-relay)
and [Hermes agent](https://github.com/nousresearch/hermes-agent) gateway webhook endpoints.
Solves firewall penetration for multi-agent deployments and zero-dependency email
inbound.

---

## Why bridge

**Pain 1 — Multi-agent firewall penetration**: Each Hermes agent's gateway webhook
runs on its own port (8645, 8646, …). Exposing them to the internet means N ports,
N firewall rules. Bridge's push mode provides a **single entry port** that
auto-routes to all gateway webhook ports — open just one port.

**Pain 2 — Zero-dependency email inbound**: No public IP? No port forwarding?
Pull mode uses a single **outbound HTTP long-poll** — bridge actively fetches
mail from relay and forwards to local gateway webhook ports. Zero inbound ports
required.

---

## Key features

### Secure transparent pass-through

Bridge holds no HMAC secrets. Relay signs with each agent's webhook secret →
bridge forwards headers & body verbatim → gateway verifies. Security boundary
unchanged. Push mode supports IP allowlist; pull mode uses ACK-based consumption
with dedup to prevent message loss.

### Lightweight zero-dependency process

Pure Rust, single binary ~3 MB. < 5 MB memory, near-zero CPU at idle.
No Python, Node, or database required. `--daemon` double-fork daemon mode
for systemd/Docker deployment. SIGINT/SIGTERM graceful drain.

### Efficient aggregated forwarding

When one email reaches multiple recipients under the same bridge, the relay
sends a **single body copy** with per-recipient headers — then bridge fans out
to each gateway webhook port. Works for both push and pull modes.

### Regex-based multi-machine forwarding

`[hosts]` table maps agent email patterns (regex) to host IPs. First-match-wins,
unmatched agents default to `127.0.0.1`. Auto-discovered from Hermes profiles,
overridable via a manual routes TOML file.

### Multi-dimensional security hardening

- **IP allowlist** — push mode only accepts POSTs from trusted relay IPs
- **Body size limit** — 10 MB cap prevents memory exhaustion
- **Graceful shutdown** — SIGINT/SIGTERM drain in-flight requests
- **Poisoned lock recovery** — `RwLock` unwrap-or-recover on all paths
- **Header filtering** — only business headers forwarded (x-amail-email,
  x-webhook-signature, x-mailrelay-timestamp, content-type)
- **TLS** — rustls-backed HTTPS with optional Let's Encrypt ACME auto-cert

### Multiple automation features

- **Zero-config routing** — auto-scans `~/.hermes/profiles/` for agent webhook
  ports via inotify hot-reload
- **ACME auto-TLS** — `tls = true` + `public_url` → automatic certificate from
  Let's Encrypt (HTTP-01 challenge). Cached, auto-renewed.
- **Batch aggregation** — relay automatically groups recipients by bridge URL
  and payload hash to minimize HTTP round-trips and bandwidth
- **Daemon mode** — `--daemon` double-fork, PID file, log file, zero manual
  supervision needed

---

## Two modes

### Push — one port, all agents

```
                       ┌─────────────────────────────────┐
                       │         amail-bridge             │
                       │  (single public port 38080)       │
relay ──POST──►        │                                  │
  alice@...+bob@...    │  alice → 127.0.0.1:8645          │────► gateway webhook:8645
  (one body copy)      │  bob   → 127.0.0.1:8646          │────► gateway webhook:8646
                       │  carol → 127.0.0.1:8647          │────► gateway webhook:8647
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
  │                                 │ fan-out to each gateway webhook  │
  │                                 │ ACK forwarded deliveries         │
  │                                 └────────────────────────────────┘
  │◄── POST /pending/ack ─────────────────────────│
```

- Single **outbound HTTP connection** to relay, fully bypasses NAT/firewall
- Same batch aggregation: one body copy for all recipients
- ACK-based consumption + 2-hour dedup cache — no messages lost, no duplicates

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
certificate from Let's Encrypt via HTTP-01 challenge:

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

### Challenge method

#### HTTP-01 (implemented)

Bridge handles everything automatically. You only need to ensure these
prerequisites are met:

1. **DNS**: the domain in `public_url` resolves to the bridge's IP
   ```bash
   dig bridge.example.com   # must return the bridge server's public IP
   ```
2. **Firewall**: port 80 is open inbound (TCP) on the bridge server
3. **Port 80 free**: no nginx, Apache, or other process listening on port 80
   (bridge binds it temporarily during challenge, then releases it)

That's it. No configuration beyond `tls = true` + `public_url`. The bridge will
automatically request the certificate, save it, and renew it.

#### DNS-01 (not yet implemented)

DNS-01 proves domain ownership by creating a `_acme-challenge` TXT record
in your DNS zone instead of serving an HTTP endpoint. How it would work:

1. Bridge contacts Let's Encrypt and receives a challenge token
2. Bridge calls your DNS provider's API to create:
   ```
   _acme-challenge.bridge.example.com   TXT   "<token>"
   ```
3. Let's Encrypt queries the TXT record to verify ownership
4. Certificate is issued; bridge cleans up the TXT record

**What you would need to configure** (not implemented):

```toml
[push]
tls = true
public_url = "https://bridge.example.com"
acme_challenge = "dns"            # "http" (default) or "dns"

[push.dns]
provider = "cloudflare"           # cloudflare / route53 / manual
api_token = "..."                 # provider-specific credentials
# zone_id = "..."                 # for Route53
```

For providers without API access, a `manual` mode would print the TXT record
value and wait for you to create it:

```
$ amail-bridge
ACME DNS-01 challenge — add this TXT record to your DNS:
  _acme-challenge.bridge.example.com   TXT   "abc123def456"
Press Enter after creating the record...
```

**Workaround today**: if port 80 is unavailable, use static certificates
(`tls_cert` / `tls_key`) or put nginx/Caddy in front of the bridge to handle
ACME.

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
| relay + gateway on same machine | Push | Bridge proxies single port to local gateway webhook ports |
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
| ACME: fallback to HTTP | Domain resolves to bridge? Port 80 reachable? `RUST_LOG=info` for ACME errors |
| ACME: DNS-01 needed | Use static certs or reverse proxy in front of bridge |
