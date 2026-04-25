# Charon × NyxID 集成

## 关系

Charon 是 **独立项目**，**不是 NyxID 的 fork**。两者通过 NyxID 已有的公开 API 协作：

- **NyxID 提供**：反向隧道、JWT 鉴权、审计、审批 API、凭证 broker、移动推送
- **Charon 提供**：workspace、agent runtime、文件 / diff / terminal API
- **集成方式**：Charon daemon 注册为 NyxID 用户的一个 `UserService`（指向 `localhost`），NyxID node 把流量转过来

**完全不需要修改 NyxID 任何代码。** 这是经过逐步代码核实后的结论，关键证据见 `03-architecture.md` 末尾的核实清单。

## 一次配置流程

假设用户在电脑 A 上配置 Charon：

```bash
# Step 1: 安装并启动 Charon daemon（绑 127.0.0.1:18789，绝不直接对外）
charon daemon install
charon daemon start

# Step 2: 把电脑 A 注册为 NyxID node
nyxid node register-token --name HostA-charon          # → nyx_nreg_xxx
nyxid node register --token nyx_nreg_xxx \
                    --url wss://auth.nyxid.dev/api/v1/nodes/ws
nyxid node daemon install && nyxid node daemon start

# Step 3: 把 Charon daemon 注册为这个 node 上的 UserService
#   实测有效命令（nyxid CLI 0.3.0）
nyxid service add --custom \
    --label charon \
    --endpoint-url http://localhost:18789 \
    --via-node <node-id> \
    --auth-method bearer \
    --credential-env CHARON_NODE_TOKEN

# Step 3.5: 启用 identity propagation（CLI 没暴露这些 flag，PUT 一下 API）
JWT=$(cat ~/.nyxid/access_token)
curl -X PUT \
    -H "Authorization: Bearer $JWT" \
    -H "Content-Type: application/json" \
    -d '{"identity_propagation_mode":"both","identity_include_user_id":true,"identity_include_email":true,"identity_include_name":false}' \
    "https://nyx-api.chrono-ai.fun/api/v1/keys/<service-id>"

# Step 3.6: 把 daemon 端的 stub credential 加到 node keychain
#   service add 之后会被要求做这一步；rpassword 要 TTY，脚本里走 expect
expect <<EOF2
spawn nyxid node credentials add --service charon --url http://localhost:18789 \
    --header Authorization --secret-format bearer
expect "value"
send "any-stub-token\r"
expect eof
EOF2

# Step 4: 在任意其它设备
#   打开 Charon Desktop → 登录 NyxID → 自动发现 charon UserService
#   建立 wss://auth.nyxid.dev/api/v1/proxy/s/charon/ws 连接
```

之后所有从客户端到 daemon 的请求都会走：

```
Client → NyxID backend → Host A 上的 nyxid node agent → localhost:18789 (charon daemon)
```

NAT 穿透、TLS 终止、JWT 鉴权、audit log 全部由 NyxID 处理。

为了缩短 Step 2-3 的链路，Charon CLI 会提供一个一键封装：

```bash
charon link               # 自动调 nyxid CLI 完成 node 注册 + UserService 创建
charon doctor             # 自检：daemon up? node connected? UserService 在? JWT 有效?
```

## 身份传播

NyxID 在转发请求到下游时会注入这一组 header（见 `proxy.rs:1405-1478`）：

| Header | 内容 |
|---|---|
| `X-NyxID-Identity-Token` | RS256 签名的 JWT，含 `user_id` 等 claims |
| `X-NyxID-Agent-Id` | 当前 API key 的 ID（用 `nyxid_ag_*` 时） |
| `X-NyxID-User-Roles` | 当前用户的角色列表 |
| `X-NyxID-User-Permissions` | 权限列表 |
| `X-NyxID-User-Groups` | 组列表 |
| `X-NyxID-Delegation-Token` | 委派访问场景 |

Charon daemon 在启动时拉取 NyxID 的 JWKS（`/.well-known/openid-configuration`），用公钥验证 JWT。所有 API 调用从 JWT 取 `user_id` 做 scoping——同一个 daemon 概念上可以服务多个 NyxID 用户（虽然 node 是 user-scoped，意味着实际只有 owner 能访问）。

## NyxID 给 Charon 的红利

| 红利 | 通过什么 API |
|---|---|
| 反向隧道（NAT 穿透） | `/api/v1/proxy/s/charon/*` 透传 HTTP + WS |
| Auth | JWT 自动注入 header，daemon 验证 |
| Audit | NyxID 自动写 `audit_logs` collection |
| 审批人在回路 | Charon daemon → `POST /api/v1/approvals`，手机推送批准 |
| 凭证 broker | spawn 出 claude / codex 进程时，从 NyxID `/api/v1/keys` 拉 `ANTHROPIC_API_KEY` 等环境变量 |
| Per-agent 隔离 | 给每个 agent 进程发独立 `nyxid_ag_*` API key，自动速率限流 |
| 移动 UI 一部分 | 审批通知用 NyxID 现有 mobile app，无需 Charon 自己做 |

## 必须工程绕开的硬约束

经核实 NyxID 代码，以下是已知约束（非 blocker，但 Charon 端必须处理）：

1. **WS idle timeout = 300s**（`backend/src/handlers/proxy.rs:2828` 硬编码）
   → Charon daemon 必须主动心跳，建议 60-90s 一次 ping
2. **Streaming idle timeout = 60s**（`PROXY_STREAM_IDLE_TIMEOUT_SECS`，env 可调）
   → 用 WS 不要用 SSE
3. **Approval policy 会拒绝 WS upgrade**（`proxy.rs:1508-1514`）
   → Charon UserService **绝不**配 approval mode
   → 细粒度审批由 Charon daemon 自己拦截工具调用，回头调 NyxID approval API
4. **`WS_PASSTHROUGH_MAX_CONNECTIONS=200`** 全局上限（不是 per-user）
   → 单用户场景无关；多用户/团队部署需评估
5. **Body 100MB 上限**（`PROXY_MAX_BODY_SIZE`，env 可调）
   → 大文件上传分块；diff 流通过 WS 帧分片不受此限

## 已 PoC 验证（2026-04-25）

WS upgrade 经过 node 路由时，`X-NyxID-*` header **会原样到达** Charon daemon。echo daemon 在 WS upgrade 后首帧 dump 收到了完整的 identity header 集合：

```
nyx_headers_seen: [
  "x-nyxid-identity-token",     // RS256 JWT, aud=http://localhost:18789, 60s TTL
  "x-nyxid-user-id",
  "x-nyxid-user-email",
  "x-nyxid-user-permissions",
  "x-nyxid-user-roles"
]
```

JWT claims 含 `sub` (user_id), `email`, `roles`, `groups`, `permissions`, `nyx_service_id`, `aud` (= endpoint URL)。Charon daemon 用 NyxID JWKS 验签即可 trust。

### 已知依赖：NyxID Issue #513

`identity_include_name=true` 配合用户 `display_name` 为非 ASCII（中文/日文/俄文/emoji 等）会让 WS upgrade 直接 500——`X-NyxID-User-Name` header 注入了原始 UTF-8 字节，触发 `tokio_tungstenite::IntoClientRequest` 的 ASCII 校验。

详情见 https://github.com/ChronoAIProject/NyxID/issues/513。

**Charon 端 workaround**：建 UserService 时 `identity_include_name: false`。`name` 仍然在 JWT body 的 claims 里安全地 base64 round-trip，daemon 从 JWT 取 name 即可——比从 header 取更可靠，所以这个 workaround 长期保留也没坏处。

### NyxID 相关的可观测性建议（potential follow-up issue）

PoC 过程发现：node 端的 `ws_proxy_error` 在 backend 被包成 `AppError::Internal`（generic 1006），原始错因丢失。`charon link` 在做 `doctor` / 健康检查时如果碰到 WS 500 internal_error，应该在 daemon 日志里把详细错因 mirror 出来，方便用户自查。NyxID 可能也应该把 ws_proxy_error 的 message 透传到响应 body（已在 #513 里附议）。

## 我们故意不做的事

为了避免污染 NyxID 的领域模型，Charon **不会**：

- 把 workspace / agent / terminal 状态塞进 NyxID 的 MongoDB
- 用 NyxID 的 catalog 系统给 Charon 自己注册（catalog 是 admin-only，不适合 per-user 自部署 daemon）
- 给 Charon 自己做 OAuth provider 注册——直接复用 NyxID 用户登录态
- 强行把"远程命令执行"塞到 NyxID 的 SSH exec 路径（语义错配）

NyxID 保持它"auth/credential/audit broker"的本色，Charon 保持它"workspace runtime"的本色，两边不变形。
