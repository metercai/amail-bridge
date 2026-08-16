# amail-bridge 审计修复计划(AUDIT-1)

审计日期: 2026-08-16 | 代码: /home/ubuntu/amail-bridge(main)
基线: cargo check 零警告,92 测试全绿。

## 发现汇总

### P1(运行时影响,修)
| # | 位置 | 问题 | 修复 |
|---|------|------|------|
| A1 | router.rs:292-318 | `writing_routes` AtomicBool 置 true 后永不复位 → 外部编辑 routes 文件不再热重载(热重载失效) | write_current_routes 写盘后复位 false(load_from_file 已复位;watcher 读 flag 判 is_our_write 即可防自触发) |
| A4 | main.rs:212-214 | daemon 退出时删 pid 文件 → 重启后 start_bridge 靠 pid 文件杀旧进程失效(双实例风险) | 删除退出时 remove_file(pid 文件保留,下次启动覆盖);同 commit 修 deploy_bridge 的 start_bridge 已有 pgrep 兜底,双保险 |

### P2(代码质量,逐项评估)
| # | 位置 | 问题 | 难度/收益 | 建议 |
|---|------|------|-----------|------|
| A3 | health.rs:61 | https 全 URL 路由探测错端口(from_url 默认 80) | 2/3 当前全 HTTP | **修**: target 用 route.target_url 解析 host+port(带 scheme 处理) |
| B1 | config.rs:159-160 | `impl PushConfig {}` 空块 | 1/1 | **删** |
| B2 | config.rs:68-71,398-402 | hosts/deserialize_hosts_vec/compiled_hosts deprecated 死代码 | 2/2 | **删**(用户定调死代码必删;git 历史可查) |
| B3 | config.rs:33-35,94-97 | acme_cache/has_tls 未接线 | 2/1 | **删**(TLS 功能未实现,留着是误导;tls_cert/tls_key 字段保留——admin.rs 引用?检查后定) |
| B4 | push.rs:309-361 | start_push_server/start_push_http 死函数 | 2/2 | **删**(main.rs start_http 是实际) |
| B5 | push.rs:470-491 | build_tls_config_from_paths 未接线 | 2/1 | **删**(TLS 未实现) |
| B6 | router.rs:322-352 | write_routes_file_with 死代码 | 2/1 | **删** |
| B8 | admin.rs:28-29 | AdminState.allowed_ips 冗余字段 | 1/1 | **删**(middleware 用独立 Vec) |
| B9 | config.rs:244-247 | PullConfig::effective_key 死代码 | 1/1 | **删**(PullSystemConfig 版本在用) |
| C1 | main.rs:42-71 vs 461-475 | parse_args/parse_args_from 重复 | 2/2 | **合并**: parse_args_from 是唯一实现,parse_args 调它(或反之) |
| C2 | push.rs 双函数 | 三份 shutdown 循环 | 2/2 | 随 B4 删除解决 |
| C3 | admin.rs:62-74 vs push.rs:73-83 | CIDR 解析重复 | 2/2 | **提取**到 security.rs 统一 parse_cidr(两处调同一实现) |
| D1 | admin.rs | admin_allowed_ips 默认空=全开放 | 3/3 | **修**: 默认 localhost 白名单(config Default + load 时空则填 127.0.0.1/::1;生产绑本地不受影响,公网部署安全) |

### P3(暂不修,记录)
| # | 位置 | 说明 |
|---|------|------|
| A2 | admin.rs:146 | port==0 校验语义(CLI 传 80 全 URL)正常,仅注释补充 |
| A5 | pull.rs:145-152 | ACK 告警阈值边界(9 次不触发)无实害,不修 |
| A6 | vhost.rs:324 | 测试固定 temp 路径,改 tempfile::TempDir(随 C3 顺带) |
| D2 | push.rs:263 | batch JSON 先解析(内存),DefaultBodyLimit 20MB 兜底,不修 |
| D3 | pull.rs:104-113 | 转发 headers 来自 relay,reqwest 处理 host 注入,不修 |
| E1 | health.rs | 批量删路由多次写盘,低频可接受,不修 |
| E2 | push.rs:128 | 每请求锁,低频可接受,不修 |

## 执行顺序
1. P1: A1(writing_routes 复位)+ A4(pid 文件保留)
2. P2 死代码清理: B1/B2/B3/B4/B5/B6/B8/B9(删)
3. P2 重构: C1(parse_args 合并)+ C3(CIDR 提取 security.rs)
4. P2 安全: D1(admin_allowed_ips 默认 localhost)
5. P2 逻辑: A3(health target 用 URL 解析)
6. 测试: cargo check 零警告 + cargo test 全绿 + 新增测试(writing_routes 复位 / admin 默认白名单 / parse_args 合并)
7. 提交推送

## 风险
- 删 hosts/compiled_hosts: 若 config.toml.example 仍引用需同步删;检查引用后定
- D1 默认 localhost: 若用户曾靠"默认全开放"远程管理(应无,生产绑本地),需提示
- 不触碰: pull 核心逻辑(转发/ACK/dedup)、push 转发路径、vhost 功能
