# amail-bridge vhost + ACME 增强方案

## 现状

- ACME: src/acme.rs 已有完整的 Let's Encrypt 证书获取/续期
- Push: src/push.rs HTTP(S) 服务器已有 TLS 支持
- 缺失: vhost 多域名路由（Static/Proxy/Redirect）

## 改动

### 1. 新增 src/vhost.rs
从 agent-mail-relay/src/advanced/vhost.rs 移植：
- VhostSiteConfig: domain + root/proxy/redirect
- VhostRoute enum: Static(PathBuf) | Proxy(String, Client) | Redirect(String)
- build_routes(), handle_vhost(), find_vhost(), load_sites_from_config()

### 2. 修改 src/config.rs
PushConfig 加 `sites: Vec<VhostSiteConfig>`，从 `[[push.sites]]` 读取

### 3. 修改 src/push.rs  
start_push_server 集成 vhost:
- 加载 sites 配置 → build_routes
- Router fallback 走 vhost 路由（find_vhost → handle_vhost）
- 原 /webhooks/* 和 /health 路由不变

### 4. 修改 src/main.rs
加 `mod vhost;`

### 5. Cargo.toml
加 `reqwest/stream` feature（Proxy 流式转发需要）

### 6. amail_bridge.toml
加 `[[push.sites]]` 示例
