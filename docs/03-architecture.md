# Charon 架构

## 系统总图

```
┌───────────────────────┐
│ Charon Desktop / 移动  │  (任意网络)
└───────────┬───────────┘
            │ HTTPS / WSS, NyxID JWT
            ▼
┌───────────────────────┐
│      NyxID Backend    │  反向隧道 + auth + audit + approval
└───────────┬───────────┘
            │ NyxID node WS frame protocol
            ▼
┌───────────────────────┐
│    nyxid node agent   │  (Host A 上)
└───────────┬───────────┘
            │ HTTP/WS to localhost:18789
            ▼
┌───────────────────────────────────┐
│         Charon Daemon             │  ← 本项目核心
│  ┌────────────────────────────┐   │
│  │ Workspace Manager          │   │
│  │ Agent Runtime              │   │
│  │ Terminal Manager           │   │
│  │ File / Diff API            │   │
│  │ MCP Loopback Server        │   │
│  │ Approval Bridge → NyxID    │   │
│  │ NyxID JWT verifier         │   │
│  └────────────────────────────┘   │
│              ↓ spawn               │
│   claude / codex / opencode /      │
│   shell processes (with PTY)       │
└───────────────────────────────────┘
```

## Daemon 内部组件

### Workspace Manager
- **抽象**：每个 workspace = 一个 git worktree + 元数据
- **存储**：`~/.charon/workspaces.json`（id, path, project_root, branch, base, created_at, archived_at）
- **生命周期**：create / list / archive；archive 级联 kill 该 workspace 关联的所有 agent + terminal
- **git 操作**：用 `gix`（gitoxide），纯 Rust 零 shell-out

### Agent Runtime
- **核心 trait**：

```rust
#[async_trait]
trait AgentClient: Send + Sync {
    async fn spawn(&self, ctx: SpawnContext) -> Result<AgentHandle>;
    async fn send_prompt(&self, agent_id: &str, prompt: &str) -> Result<()>;
    async fn wait(&self, agent_id: &str) -> Result<AgentOutcome>;
    async fn kill(&self, agent_id: &str) -> Result<()>;
}
```

- **provider 适配器**：
  - **Claude**：spawn `claude` CLI，注入 `CLAUDE_MCP_SERVERS=http://127.0.0.1:18789/mcp`，环境变量带 `ANTHROPIC_API_KEY`（启动前从 NyxID `/api/v1/keys` 拉一次性缓存）
  - **Codex**：spawn `codex` CLI 或调 codex appserver
  - **OpenCode**：spawn `opencode` CLI
- **输出捕获**：stdout/stderr 走 PTY，分流到 `agent.timeline` event stream（用 `tokio::sync::broadcast`）
- **状态机**：`Starting → Running → (WaitingPermission|Finished|Errored)`

### Terminal Manager
- **PTY 池**：每个 terminal 有 id + workspace_id + scrollback ring buffer (16K lines)
- **依赖**：`pty-process` crate（NyxID CLI 已用，模式可参考 `cli/src/node/ws_client.rs`）
- **支持**：resize、特殊键序列（Enter / Tab / C-c / C-d）、颜色保留

### File / Diff API
- **路径**：所有文件操作必须在 workspace_id 对应的 worktree 内（防越界，统一走 `realpath` 校验前缀）
- **Diff**：`gix` 计算 working-tree vs base，结果按文件分块发送；用 `notify` 监听文件变化触发增量推送
- **大文件**：>1MB 走分片帧（避免单帧爆掉 16MB WS 限制）

### MCP Loopback Server
- 同一个 daemon 进程内的 MCP server，挂在 `/mcp` 路径
- spawn'd agent 通过 `CLAUDE_MCP_SERVERS` 环境变量发现
- 暴露的工具命名：`charon.workspace.create_worktree`、`charon.agent.create`、`charon.terminal.send_keys`、`charon.list_pending_permissions`、`charon.respond_to_permission` 等
- **这就是 paseo 的 killer feature**——agent 可以反过来调 daemon 编排其它 agent / 操作 workspace

### Approval Bridge
- 拦截 agent 的危险工具调用（写文件 / 跑 shell / 外网请求）
- **本地 trust policy 先过滤**：workspace 内 Read 自动放行、`./node_modules` 写自动放行等
- **真正高危的调 NyxID**：`POST /api/v1/approvals`，阻塞等用户在手机上批准
- 决策结果回写 audit，本地缓存"该 workspace 内同类操作 N 分钟内自动放行"

### NyxID JWT Verifier
- 启动时拉取 `/.well-known/openid-configuration` → `jwks_uri` → 公钥
- 缓存 24h，过期 lazy 刷新
- 每个进入 daemon 的请求验 `X-NyxID-Identity-Token`，从 claims 取 `sub` (user_id)

## Wire Protocol (Client ↔ Daemon)

WebSocket 上的 JSON 帧 + 可选 binary attachment：

```json
{ "type": "Workspace.Create",  "id": "req-123", "payload": { ... } }
{ "type": "Workspace.Created", "id": "req-123", "result":  { ... } }

{ "type": "Agent.Spawn", "id": "req-456",
  "payload": { "provider": "claude", "workspace_id": "ws-1", "model": "opus-4.7" } }

{ "type": "Agent.TimelineEvent",   // server-pushed
  "agent_id": "ag-789", "event": { "kind": "tool_use", ... } }

{ "type": "Terminal.Output", "terminal_id": "t-1",
  "binary_attachment_ref": "att-1" }   // 紧跟一个 binary frame，header 带 ref
```

**Frame 命名空间**：

- `Workspace.*`：create / list / archive / select
- `Agent.*`：spawn / send-prompt / wait / kill / list-pending-permissions / respond-permission
- `Terminal.*`：create / send-keys / capture / resize / kill
- `File.*`：read / write / list / watch / unwatch
- `Diff.*`：subscribe / unsubscribe / refresh
- `Mcp.*`：passthrough（client 想直接调 daemon MCP 工具）

二进制载荷（PTY 输出、文件内容、diff blob）走 WS binary frame，载荷前 36 字节是 envelope 里的 attachment ref UUID（参考 NyxID node frame 协议的 36B 前缀模式）。

## 端口 / 绑定

- **Daemon**：`127.0.0.1:18789`（localhost only，**绝不直接对外**）
- **MCP loopback**：同端口，路径 `/mcp`
- 唯一对外入口：通过 NyxID node agent 转发

## 桌面客户端（Charon Desktop）

- **跨平台**：macOS / Linux / Windows
- **登录流程**：
  1. 打开 app → 配置 NyxID base URL（默认 `https://auth.nyxid.dev`）
  2. OAuth / 密码登录获取 JWT
  3. 调 `GET /api/v1/user-services` 列出当前用户的 UserServices
  4. 找到 `name == "charon"` 的项，记录其 slug
  5. 建立 WS：`wss://auth.nyxid.dev/api/v1/proxy/s/{slug}/ws?token=<jwt>`
- **UI 模块**：workspace 树、agent timeline、terminal 视图、diff 视图、文件编辑器
- **多设备同步**：JWT + UserService discovery 是无状态的，同一账号在多设备登录互不干扰

移动端：M3 阶段考虑，复用同一套 WS 协议。

## 我们核实过 NyxID 代码的关键点（已 PoC 端到端验证）

| 步骤 | 状态 | 关键证据 |
|---|---|---|
| Node user-scoped 注册 | ✅ | `models/node.rs:69` `Node.user_id: String` |
| UserEndpoint 接受 localhost URL | ✅ | `models/user_endpoint.rs:12` 注释明示 + 全代码无 SSRF 校验 |
| HTTP 透传到 localhost | ✅ | `proxy.rs:1626-1636` + `cli/src/node/proxy_executor.rs:166`；PoC 实测 200 OK，body + path + query 正确 |
| WS 透传（via node） | ✅ | `proxy.rs:1504-1557` 走 `handle_ws_passthrough_via_node`；PoC 实测 upgrade + 双向 text 帧 OK |
| 身份 header 注入（HTTP + WS 都到达 daemon） | ✅ | `proxy.rs:1405-1478` 调 `identity_service::build_identity_headers`；PoC 实测 5 个 header 全部到达 echo daemon |
| `X-NyxID-Identity-Token` JWT 含 user_id/aud/roles/permissions | ✅ | `identity_service.rs:94-138`；aud = endpoint URL，60s TTL |
| Approval blocks WS | ⚠️ | `proxy.rs:1508-1514`——Charon UserService 不能配 approval |
| Body 100MB 上限 | ✅ | `config.rs:673-676`（env 可调） |
| WS idle 300s | ⚠️ | 硬编码；daemon 要心跳 < 300s |
| `identity_include_name` + 非 ASCII display name | ❌ | NyxID#513；workaround：`identity_include_name: false`，name 从 JWT claims 取 |

## 仓库结构（建议）

```
charon/
├── Cargo.toml              # workspace
├── crates/
│   ├── charon-core/        # workspace / agent / terminal / file / diff 模型与接口
│   ├── charon-daemon/      # 长进程，axum server，组装 core 模块
│   ├── charon-cli/         # `charon` 命令行
│   ├── charon-mcp/         # MCP loopback server
│   ├── charon-nyxid/       # NyxID API client：JWT 验证、API 调用、approval 桥
│   └── charon-desktop/     # Tauri app（src-tauri Rust + 前端 TypeScript）
└── docs/
```

## Roadmap 雏形

| 里程碑 | 范围 |
|---|---|
| **M1 Hello World** | daemon + axum WS server + 单 workspace + spawn `claude` + 经 NyxID proxy 跑通端到端 |
| **M2 真实 UX** | git worktree、文件 / diff / terminal、Tauri desktop client 的 timeline 视图 |
| **M3 多 agent + MCP 回环** | provider registry、MCP loopback、approval bridge、多 agent 编排 |
| **M4 移动 / schedule** | 移动 UI、scheduled agent、多 workspace 并发 |
