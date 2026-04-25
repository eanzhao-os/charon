# PoC 结果与下一步计划

> 本文档是从 NyxID workspace 切换到 charon workspace 的交接单。读完这一份就能在新会话里接着干。

## TL;DR

**Charon-on-NyxID 的全链路（client → NyxID hosted → node agent → localhost daemon）已 PoC 验证打通**——HTTP + WebSocket 双协议、身份传播、JWT 验签、双向 text/binary 帧。**不需要改 NyxID 任何代码。** 唯一外部依赖：[NyxID#513](https://github.com/ChronoAIProject/NyxID/issues/513)（非 ASCII display name 触发 WS upgrade 失败），workaround = `identity_include_name: false`，长期保留。

## 已验证清单

| 维度 | 结果 |
|---|---|
| 反向 WS 隧道穿 NAT，从任意网络打到 dev 机的 daemon | ✅ |
| HTTP 任意方法 / 任意 path / query / body 透传 | ✅ |
| WS upgrade + 双向 text + 双向 binary 帧 | ✅ |
| `X-NyxID-Identity-Token` (RS256 JWT) 在 HTTP 和 WS 都到达 | ✅，`aud` 字段 = endpoint URL |
| `X-NyxID-User-Id / -Email / -Roles / -Permissions` 到达 | ✅ |
| Node-managed credential 注入到下游请求 | ✅ |
| 多 client 并发连接（多设备登录同账号） | 隔离机制存在但 PoC 没拉到极限 |

详细帧编号、JWT payload 例子、rate limit 数字都在 03-architecture.md 末尾的"核实清单"里。

## 现存的 PoC 资产（可复用 / 可丢）

跑 PoC 时建立的东西，**默认保留**，下次回归测试可以直接用：

1. **本地 echo daemon**：`~/Code/charon/poc/echo-daemon/`，跑在 `127.0.0.1:18789`（`PID 95171`，重启后会丢，cargo run 重启）。任何 HTTP / WS 请求都会 dump 收到的 header 列表。
2. **NyxID UserService**：slug = `charon-echo-poc`，service_id = `a71914da-6eba-4b4d-86ed-fd0f455bb79f`，路由经 node `7cf645a2-08eb-40c0-950d-ed02efb4a142`（这台 mac-mbp）。`identity_propagation_mode = both`，`identity_include_name = false`。
3. **Node keychain credential**：service `charon-echo-poc`，stub Authorization Bearer。
4. **本机 nyxid CLI**：升级到了 0.3.0（之前是 0.1.0，是为了拿到 WS-via-node 支持升的）。

要全部清理的话：

```bash
# 杀 echo daemon
kill 95171   # 或 pkill echo-daem

# 删 NyxID 上的 service
nyxid service delete charon-echo-poc

# 删 node 本地 credential
nyxid node credentials remove --service charon-echo-poc

# 想保留 nyxid 0.3.0 binary 不用动
```

## 本机当前状态须知

- **nyxid node launchd daemon**：PoC 中我们 `nyxid node daemon stop` 过它，因为新二进制签名变了 keychain ACL。你重启 daemon 会大概率再次卡 keychain。当前可能的状态：你 terminal 里手动跑着的 `nyxid node start`。
  - 想恢复 launchd 模式：先在 Keychain Access UI 把 `nyxid-node` 那几条都 "Always Allow" 给新签名的 nyxid binary，或者 `nyxid node daemon restart` 后看是否 stuck（stuck 就回去用 foreground）。
  - 这条只影响你的日常 NyxID node 流量（GitHub / OpenAI 等代理），不影响 charon。
- **nyxid 二进制**：`~/.cargo/bin/nyxid` 0.3.0，是从 NyxID main `3279d9c` 编出来的。NyxID 树本身 clean。

## 在新 charon workspace 的 Claude Code 会话里给 AI 的初始上下文

新会话开了之后，把下面这段塞进首条消息（或写进 CLAUDE.md）：

> Charon 是一个跑在 dev 机器上的 Rust daemon，让任意设备通过 NyxID 反向隧道远程使用本机资源跑 Claude Code / Codex / OpenCode 等 AI coding agent。设计文档全在 `docs/`，按数字顺序读：01 overview → 02 nyxid 集成 → 03 架构 → 04 技术栈 → 05 PoC 结果 + M1 计划（本文）。
>
> 关键事实：
> - 不改 NyxID 任何代码，把 Charon daemon 注册成 NyxID 用户的一个 UserService 即可
> - daemon 绑 `127.0.0.1:18789`，**绝不**直接对外
> - Tauri v2 桌面客户端（Rust 后端 + React 前端，复用 NyxID frontend 的 React/TanStack/Tailwind 组件库）
> - 状态本地优先，**不**塞进 NyxID MongoDB
> - PoC 通过的端到端调用路径：`client → wss://nyx-api.chrono-ai.fun/api/v1/proxy/s/charon/ws → nyxid node → localhost:18789`

---

## M1 — 最小骨架（目标：替换 echo daemon，跑通端到端）

**Definition of Done**：从任意机器 `curl -H "Authorization: Bearer <JWT>" https://nyx-api.chrono-ai.fun/api/v1/proxy/s/charon-echo-poc/api/v1/health` 返回 charon daemon 自己写的 200 + JSON `{ok: true, user_id: ...}`（user_id 从 X-NyxID-Identity-Token 验签后取出）。

### M1 任务列表

1. **Workspace 骨架**
   - `cargo new --bin charon` 建 workspace，移到 `~/Code/charon/`（注意：现在 `~/Code/charon/` 已经有 `docs/` 和 `poc/`，工程在根加 `Cargo.toml` workspace 即可）
   - 三个 crate：`crates/charon-core`、`crates/charon-daemon`、`crates/charon-cli`
   - workspace `Cargo.toml` 里把 axum 0.8 / tokio / tracing / serde / clap / anyhow / thiserror / reqwest 这些 dep 写到 `[workspace.dependencies]`，子 crate 用 `<dep>.workspace = true`

2. **`charon-daemon` 最小 server**
   - `axum::Router` 监听 `127.0.0.1:18789`
   - `GET /api/v1/health` → 解析 `X-NyxID-Identity-Token`，返回 `{ok: true, user_id, email}`
   - `GET /api/v1/whoami` → 同上但更详细
   - `tracing-subscriber` 输出结构化日志

3. **JWT 验签模块（`charon-core::nyxid_jwt`）**
   - 启动时拉 `https://nyx-api.chrono-ai.fun/.well-known/openid-configuration` → 取 `jwks_uri`
   - 拉 JWKS，缓存 24h
   - `verify(token, expected_aud)` → 返回 claims struct
   - axum extractor `IdentityToken`：从 `X-NyxID-Identity-Token` 拿，验签，注入 handler

4. **`charon-cli` 最小命令**
   - `charon daemon start` → 起 daemon（支持 `--bind` 默认 127.0.0.1:18789）
   - `charon daemon status` → 简单 ping
   - `charon doctor` → 串起来检查：daemon up / node connected / UserService 存在 / 端到端 200（用本机调 nyxid CLI 拿 JWT，自己打 proxy）

5. **替换 echo daemon 端到端测**
   - `cargo run -p charon-daemon` 起来
   - 上面那个 PoC 的 UserService（slug `charon-echo-poc`）endpoint_url 已经是 `http://localhost:18789` —— 直接打 `proxy/s/charon-echo-poc/api/v1/whoami` 就该返回 charon 写的响应
   - 健康路径打通后把 echo daemon 退役（或保留作 regression smoke）

6. **CI / lint / 格式**
   - `rustfmt.toml`（可不写，用默认）
   - `clippy` warn-as-error 在 dev 上可选
   - 一个 GitHub Action：`cargo build --workspace`、`cargo clippy --workspace -- -D warnings`、`cargo test --workspace`

### M1 启动命令清单

新 workspace 里第一个 prompt 大致这样：

```
按 docs/05-poc-results-and-next-steps.md 的 M1 任务列表干活。
先建 workspace 骨架（Cargo.toml + 三个 crate），再写 charon-daemon 的
最小 axum server（GET /api/v1/health 返回 {ok:true}），
然后 cargo run 起来，
我用 curl 经 NyxID proxy 验证。
不要现在就做 JWT 验签，等骨架通了再加。
```

预计工作量：2-4 小时，含 rust 编译时间。

---

## M1 实施进度（live log）

| 子任务 | 状态 | 备注 |
|---|---|---|
| #1 Workspace 骨架 | ✅ 2026-04-25 | `Cargo.toml` (resolver=3, edition 2024, rust-version 1.93) + `crates/{charon-core,charon-daemon,charon-cli}` |
| #2 charon-daemon 最小 axum server | ✅ 2026-04-25 | `GET /api/v1/health` + `GET /api/v1/whoami`，response shape 在 `charon-core::wire`；`/health` 永久 anonymous |
| #3 JWT 验签模块 | ✅ 2026-04-25 | `charon-daemon::nyxid_jwt`：discovery + JWKS fetch、kid cache、`Validation` 校验 iss/aud/exp（30s leeway）、kid miss 时单次重拉、axum extractor `IdentityToken` 401/503 区分错因 |
| #4 charon-cli 最小命令 | ✅ 2026-04-25 | `charon daemon start \| status` + `charon doctor`；doctor 三段：本地 `/health`、`nyxid node daemon` 进程、经 NyxID proxy `/whoami` 端到端（读 `~/.nyxid/access_token` 拼 Bearer） |
| #5 替换 echo daemon 端到端测 | ✅ 2026-04-25 | echo daemon (PID 95171) 已 kill；`charon-daemon` 接管 `127.0.0.1:18789`；`https://nyx-api.chrono-ai.fun/api/v1/proxy/s/charon-echo-poc/api/v1/health` → 200 charon JSON |
| #6 CI / lint / 格式 | ✅ 2026-04-25 | `.github/workflows/ci.yml`：`cargo fmt --check` + `clippy --workspace --all-targets -- -D warnings` + `build` + `test`；本地 0 warning / 1 test pass |

### 与原计划的偏差

1. **JWT 验签模块的归属**：原计划 `charon-core::nyxid_jwt`，实际放到 `charon-daemon::nyxid_jwt`。理由：JWKS client 需要 `reqwest` + `tokio::sync` + `tracing`，把这些依赖塞进 `charon-core` 会让一个本应"纯 wire 类型"的 crate 变成事实上的 runtime crate。`charon-core` 留给跨 crate 共享的 serde 类型（`HealthResponse` / `WhoAmIResponse` / `IdentityDetail`）；charon-cli / charon-desktop 不需要复用 verifier 本身。如果以后 cli 真的要 decode JWT，提到 `charon-core` 也容易。
2. **`charon-daemon` 拆 lib + bin**：`src/lib.rs` 暴露 `pub async fn serve(config, shutdown)`；`src/main.rs` 是薄壳；`charon-cli` 直接 `charon_daemon::serve(...).await`，不 spawn 子进程。等 M2 上 launchd / systemd 时再加 `daemon install`。

### `charon doctor` 输出示例

```
== charon doctor ==

[1] local charon-daemon at http://127.0.0.1:18789
  ✓ charon-daemon 0.0.1

[2] nyxid node daemon
  ✓ running (PID 69251)

[3] end-to-end via NyxID proxy
    GET https://nyx-api.chrono-ai.fun/api/v1/proxy/s/charon-echo-poc/api/v1/whoami
  ✓ user_id=5d0d7b72-acff-49af-bb1b-9f30bbb7c102 email=eancuznaivy@gmail.com
    roles=["ornn-user"] permissions=6 groups=0 expires_at=2026-04-25 14:07:59 UTC

All checks passed.
```

失败模式实测：本地 daemon 不在 → `[1] ✗ Connection refused`；slug 不存在 → `[3] ✗ HTTP 404 Service not found`。两种都按段单独失败、exit 1。

### env 控制点速查

| env | 默认值 | 谁用 |
|---|---|---|
| `CHARON_BIND` | `127.0.0.1:18789` | daemon 监听地址 |
| `CHARON_EXPECTED_AUD` | `http://localhost:18789` | JWT `aud` 校验值（必须 == UserService endpoint URL） |
| `CHARON_NYXID_ISSUER` | `https://nyx-api.chrono-ai.fun` | OIDC discovery / JWT `iss` 校验值 |
| `CHARON_OWNER_USER_ID` | 无，必填 | daemon 单 owner 授权；workspace/file/diff/WS 只接受 JWT `sub == owner` |
| `CHARON_ENDPOINT` | `http://127.0.0.1:18789` | doctor 探本地 daemon |
| `CHARON_NYXID_BASE_URL` | `https://nyx-api.chrono-ai.fun` | doctor 走的 proxy 根 URL（M1 等于 issuer） |
| `CHARON_DOCTOR_SLUG` | `charon-echo-poc` | doctor 经 proxy 打的 UserService slug |

第一次升级后如果还不知道自己的 NyxID `user_id`，可以先用临时值启动 daemon，只打 `/whoami` 拿真实值，再改成真实 owner 重启：

```bash
CHARON_OWNER_USER_ID=bootstrap cargo run -p charon-daemon
curl -H "Authorization: Bearer $(cat ~/.nyxid/access_token)" \
  https://nyx-api.chrono-ai.fun/api/v1/proxy/s/charon-echo-poc/api/v1/whoami
```

### #3 端到端验证

| 测试 | 期望 | 结果 |
|---|---|---|
| 启动时拉 JWKS | 1 个 RS256 key 装进 cache | ✅ `JWKS loaded ... key_count=1` |
| `/health` 直连无 JWT | 200 anon | ✅ |
| `/whoami` 直连无 header | 401 `missing_identity_token` | ✅ |
| `/whoami` 直连乱写 token | 401 `invalid_token` (Malformed) | ✅ |
| `/whoami` 经 proxy 真实 JWT | 200 + 完整 `NyxIdentity` | ✅ user_id / email / roles / permissions / nyx_service_id / iat / exp 全活 |
| `/health` 经 proxy 真实 JWT | 200 anon（保持公开） | ✅ |

`POST .../proxy/s/charon-echo-poc/api/v1/whoami` 实际响应：
```json
{"ok":true,"version":"0.0.1","identity":{
  "user_id":"5d0d7b72-acff-49af-bb1b-9f30bbb7c102",
  "email":"eancuznaivy@gmail.com",
  "roles":["ornn-user"],
  "permissions":["ornn:skill:build","ornn:playground:use","ornn:skill:delete","ornn:skill:create","ornn:skill:update","ornn:skill:read"],
  "nyx_service_id":"a71914da-6eba-4b4d-86ed-fd0f455bb79f",
  "issued_at":"2026-04-25T13:58:22Z","expires_at":"2026-04-25T13:59:22Z"
}}
```

`name` / `groups` / `agent_id` 都没出现：name 走 `identity_include_name=false` workaround（per #513），groups 是空数组（serde 跳过），agent_id 只在 `nyxid_ag_*` API key 调用时才有。符合预期。

### 关键运行时事实（截至 2026-04-25）

- nyxid node launchd daemon：`nyxid node daemon start` 重启成功，PID 69251；docs 04-bullet 提到的 keychain stuck 这次没复现。
- NyxID UserService 没改名，仍是 `charon-echo-poc`（slug = service_id `a71914da-6eba-4b4d-86ed-fd0f455bb79f`），endpoint `http://localhost:18789`。`charon` 这个名字 M1 不抢，纯外观，等 desktop client 上线再改。
- 实际抓到的 JWT claims 例子（用于 #3 实现的契约）：
  - `iss=https://nyx-api.chrono-ai.fun`
  - `aud=http://localhost:18789`（== UserService endpoint_url）
  - `sub`=user UUID
  - `email`、`roles`、`groups`、`permissions`、`nyx_service_id`
  - `exp - iat = 60s`，每请求新签
- JWKS：`https://nyx-api.chrono-ai.fun/.well-known/jwks.json`，单个 RSA key，`kid=22fef2306d43e8fd`，alg=RS256。Discovery 在 `https://nyx-api.chrono-ai.fun/.well-known/openid-configuration`。

---

## M1 之后

- **M2 真实 UX**：git worktree workspace 管理 + 文件 / diff / terminal API + Tauri 桌面 client 的 timeline 视图
- **M3 多 agent + MCP 回环**：provider registry、MCP loopback server 注入到 spawn 出的 claude / codex 进程、approval bridge → NyxID 审批 API
- **M4 移动 / schedule**：移动 UI、scheduled agent、多 workspace 并发

详见 03-architecture.md 末尾 roadmap。
