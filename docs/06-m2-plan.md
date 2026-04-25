# M2 — 真实 UX

> 跟 docs/05 同样体例：上半部分是计划+决策，下半部分是 live progress log。

## 目标

把 M1 的 health/whoami 骨架长成"客户端能真的开 workspace、写文件、看 diff、跑 terminal"的最小可用形态。完成后桌面客户端能从手机/咖啡厅 MacBook 经 NyxID proxy 操作一台远端 dev 机的代码。

## 分块（按依赖顺序）

| 子里程碑 | 范围 | 依赖 | 状态 |
|---|---|---|---|
| **M2.1 Workspace 模型** | git worktree manager（shell-out 到 `git`）、`~/.charon/workspaces.json` 持久化、`POST/GET /api/v1/workspaces`、`POST /api/v1/workspaces/:id/archive`，全部走 `IdentityToken` | M1 | ✅ 2026-04-25 |
| **M2.2 WS 协议** | `GET /api/v1/ws` 升级（JWT at upgrade only）；JSON envelope；帧命名空间起步 `Workspace.*`；60s 服务端 Ping；Hello 帧带 identity；Error 帧带原 request id；二进制 attachment 留给 M2.3/M2.4 真用得着再做 | M2.1 | ✅ 2026-04-25 |
| **M2.3 File / Diff API** | 路径 sandbox（lexical `safe_join`，拒 `../` + 绝对路径）；`File.{List,Read,Write}` REST + WS（UTF-8 only，binary 留 M2.4）；`Diff.Get` shell-out `git diff` + `ls-files --others`，per-file unified diff | M2.2 | ✅ 2026-04-26 |
| **M2.4 Terminal** | `pty-process` 起 shell；scrollback 16K 行 ring buffer；resize / 特殊键 / 颜色保留；多 terminal/workspace | M2.2 | ⏳ |
| **M2.5 Tauri desktop shell** | `crates/charon-desktop`：Tauri v2 + React 19 + TanStack + Tailwind 4；NyxID OAuth in webview → UserService 发现 → WS 连接；workspace 树 / diff 视图 / xterm.js terminal | M2.2-M2.4 | ⏳ |
| **M2.6 `charon link` + 配置收尾** | 自动化 `nyxid node register` + `nyxid service add`，把 slug/credential 写到 `~/.charon/config.toml`；`charon doctor` 改读 config 而不是 `DEFAULT_USER_SERVICE_SLUG` | 任意时机 | ⏳ |

预估：M2.1 + M2.2 + M2.6 共 3-5 天；M2.3 + M2.4 各 2-3 天；M2.5 是大头 1-2 周看 UI 复杂度。整体 3-4 周。

## 关键决策（已定）

1. **gix 还是 git2 还是 shell-out**：worktree add/remove 直接 shell-out 到系统 `git`（gix 还没完整支持，git2 拖了 libgit2 native dep）。读端（status / blob / diff）走 gix，纯 Rust。
2. **Worktree 落点**：`~/.charon/worktrees/<workspace-uuid>/`，集中管理。M3 再考虑用户自定义路径。
3. **Workspace ID**：UUID v4 完整字符串，不裁剪（避免冲突，前端要短可自己截）。
4. **Branch 命名**：用户没提供 `new_branch` 时默认 `charon/<workspace-uuid 前 8 位>`。
5. **Archive 语义**：M2.1 只是 soft delete（设 `archived_at`），worktree 留在硬盘。M3 加 hard delete + 关联 agent/terminal 级联 kill。
6. **WS 鉴权粒度**：只在 upgrade 时验 JWT 一次（trust 整条 WS 连接）。NyxID identity token 60s TTL — 长连接靠客户端定期重连刷 token，daemon 端不用主动验。
7. **Tauri vs Dioxus**：Tauri v2 锁定（共享 NyxID frontend 组件库）。
8. **多用户**：M2-M3 假定单用户（owner == 启动 daemon 的人）。请求里 `X-NyxID-User-Id` ≠ owner 直接 403。M4 再做多用户。

## env 新增（M2 阶段会陆续加）

| env | 默认 | 谁用 |
|---|---|---|
| `CHARON_HOME` | `~/.charon` | workspaces.json + worktrees/ + (M2.6) config.toml 都在这下面 |
| `CHARON_OWNER_USER_ID` | （从 M2.6 config.toml 读） | 单用户校验 — 只接受 JWT `sub == owner` 的请求 |

---

## M2.1 实施进度（live log）

| 步骤 | 状态 | 备注 |
|---|---|---|
| Workspace 等 wire 类型 | ✅ | `charon-core::wire`：`Workspace` / `CreateWorkspaceRequest` / `ListWorkspacesResponse` |
| WorkspaceManager + JSON store | ✅ | `<CHARON_HOME>/workspaces.json`，atomic write (tmp + rename) |
| Git worktree shell-out | ✅ | `tokio::process::Command`，`git -C <project> worktree add -b <branch> <path> <base>`；project_root 必须 absolute + `git rev-parse --is-inside-work-tree` 验证；branch 默认 `charon/<id 前 8 位>`、base 默认 `git rev-parse --abbrev-ref HEAD` |
| HTTP handlers + AppState 扩展 | ✅ | `POST/GET /api/v1/workspaces`、`GET /api/v1/workspaces/:id`、`POST /api/v1/workspaces/:id/archive`，全部走 `IdentityToken` 强制鉴权；`?include_archived=true` query 切换列表过滤；`ErrorBody` 提到 `crate::ErrorBody` 共享 |
| 新增 env | ✅ | `CHARON_HOME`，默认 `$HOME/.charon` |
| 单测（charon-daemon） | ✅ | `create_list_get_archive_roundtrip` / `rejects_relative_project_root` / `rejects_non_git_dir`，3 个全过 |
| 端到端经 NyxID proxy | ✅ | create (201) → list active (1) → get (200) → archive (200) → list active (0) → list include_archived (1) → re-archive idempotent (同 archived_at) → 错误路径 401/404/400 全对 |

## M2.2 实施进度（live log）

| 步骤 | 状态 | 备注 |
|---|---|---|
| Frame wire 类型 | ✅ | `charon-core::wire`：`ClientFrame` (4 个)、`ServerFrame` (6 个)、`HelloPayload`、`ServerInfo`、`WsError`、`WorkspaceListPayload`、`WorkspaceIdPayload`；`WS_PATH = "/api/v1/ws"` 常量 |
| Daemon WS handler | ✅ | `charon-daemon::ws::ws_upgrade_handler`；`IdentityToken` extractor 在 upgrade 之前跑（401 if missing）；`run_session` 推 Hello → loop dispatch + 60s server Ping |
| Frame 分发 | ✅ | `Workspace.{List,Create,Get,Archive}` 4 个全实现，复用 `WorkspaceManager` 业务逻辑；malformed frame → `Error` (id=null, code=`malformed_frame`)；二进制帧 → `Error (unsupported_binary)` 但保连接 |
| Doctor [4] | ✅ | 改用 wss schema 拼 URL；连接 → 收 Hello → 发 Workspace.List → 收 Workspace.Listed (id 校验) → close；Hello identity 与 [3] HTTP /whoami 比对，"matches /whoami" 标注 |
| 端到端实测 | ✅ | 见下文 |

### M2.2 端到端实测（2026-04-25）

`charon doctor` 完整输出：

```
== charon doctor ==

[1] local charon-daemon at http://127.0.0.1:18789
  ✓ charon-daemon 0.0.1
[2] nyxid node daemon
  ✓ running (PID 69251)
[3] end-to-end /whoami via NyxID proxy
    GET https://nyx-api.chrono-ai.fun/api/v1/proxy/s/charon-echo-poc/api/v1/whoami
  ✓ user_id=5d0d7b72-... email=eancuznaivy@gmail.com
[4] WS handshake via NyxID proxy
    wss://nyx-api.chrono-ai.fun/api/v1/proxy/s/charon-echo-poc/api/v1/ws
  ✓ Hello received: user_id=5d0d7b72-... (matches /whoami)

All checks passed.
```

Daemon 端日志：

```
INFO charon_daemon::ws: WS upgrade user_id=5d0d7b72-...
DEBUG charon_daemon::ws: WS close from client
```

WS 鉴权 sanity（直连无 JWT）：

```bash
curl -i -H "Connection: Upgrade" -H "Upgrade: websocket" \
  -H "Sec-WebSocket-Version: 13" -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" \
  http://127.0.0.1:18789/api/v1/ws
# HTTP/1.1 401 Unauthorized
# {"error":"missing_identity_token","message":"missing X-NyxID-Identity-Token header"}
```

JWT 验签发生在 `IdentityToken` extractor，axum 的 `WebSocketUpgrade` 也是 `FromRequestParts`，二者按 handler 签名顺序串起来执行 → upgrade 之前就被挡掉。

### 跟原计划的偏差

- **二进制 attachment 36B UUID 前缀延后**：M2.2 没用例（终端输出 / 文件 blob 都是 M2.3/M2.4 的事），所以协议帧里没有 `binary_attachment_ref` 字段。等到 M2.3 加 `Diff.Blob` / M2.4 加 `Terminal.Output` 时一起补，wire 类型 enum 加变体即可向后兼容。
- **应用层 Ping/Pong 不做**：直接用 WebSocket 协议级 Ping（每 60s 服务器主动发），客户端 / NyxID node 自动回 Pong，不污染 JSON envelope。
- **没起 `charon ws probe` 子命令**：直接把 WS 探测塞进 `doctor` 第 4 步，避免新命令 + 重复实现。等 desktop client 上来了再考虑独立的 `charon ws connect` REPL。

## M2.3 实施进度（live log）

| 步骤 | 状态 | 备注 |
|---|---|---|
| File / Diff wire 类型 | ✅ | `FileEntry` / `FileKind` / `FileTreeResponse` / `FileContent` / `WriteFileRequest` / `DiffResponse` / `FileDiff` / `DiffStatus` + 4 个 WS payload 类型；`ClientFrame` +4、`ServerFrame` +4 |
| `charon-daemon::files` | ✅ | `safe_join`（lexical，拒 `..` 越界 + 拒绝对路径 + 跳过 `.` 当前目录）、`tree`（BFS + depth limit + 跳 `.git`）、`read`（UTF-8 only，明确报错 fallback 给 M2.4）、`write`（自动 `create_dir_all` parent）；6 个单测全过 |
| `charon-daemon::diff` | ✅ | shell-out `git -C <wt> diff --name-status <base>` 拿改动列表，per-file `git diff <base> -- <path>` 拿 unified diff；`git ls-files --others --exclude-standard` 拿 untracked |
| REST handlers | ✅ | `GET /workspaces/{id}/tree?path=&depth=`、`GET/PUT /workspaces/{id}/file?path=`、`GET /workspaces/{id}/diff`，全 `IdentityToken` 鉴权；`fetch_workspace_or_404` 助手统一 404 |
| WS dispatch | ✅ | `File.List → File.Listed`、`File.Read → File.Content`、`File.Write → File.Written`、`Diff.Get → Diff.Snapshot`，复用 `files` / `diff` 业务逻辑 |
| 端到端实测 | ✅ | 见下文 |

### M2.3 端到端实测（2026-04-26）

测试 repo：`/tmp/charon-m23-test`（init -b main，1 commit 含 `README.md` + `src/main.rs`）。

| # | 操作 | 结果 |
|---|---|---|
| 1 | POST 创建 workspace 指向该 repo | ✅ 201，分配 `charon/1ee3814b` 分支 |
| 2 | GET tree (depth=1) | ✅ `[README.md, src/]` |
| 3 | GET tree (depth=5) | ✅ 加上 `src/main.rs` |
| 4 | GET file `src/main.rs` | ✅ 原始内容 |
| 5 | PUT file `src/main.rs` (改) | ✅ 200 |
| 6 | PUT file `src/util.rs` (新建) | ✅ 200，自动 mkdir parent |
| 7 | PUT file `../escape.txt` (sandbox) | ✅ 400 `path '../escape.txt' escapes workspace` |
| 8 | GET diff | ✅ `changed: [{path: src/main.rs, status: modified, unified_diff: "..."}]`、`untracked: [src/util.rs]` |
| 9 | GET file 不存在路径 | ✅ 400 `read_failed` |
| 10 | GET tree on a file path | ✅ 400 `is not a directory` |
| 11 | GET diff on 不存在 workspace | ✅ 404 |
| 12 | doctor 4 步全过（WS 复测） | ✅ |

`diff` 实际响应（精简）：

```json
{
  "workspace_id": "1ee3814b-...",
  "base_branch": "main",
  "changed": [{
    "path": "src/main.rs",
    "status": "modified",
    "unified_diff": "diff --git a/src/main.rs b/src/main.rs\nindex 7b16f1f..acb2da2 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1,3 @@\n-fn main() { println!(\"hi\"); }\n+fn main() {\n+    println!(\"hi from charon\");\n+}\n"
  }],
  "untracked": ["src/util.rs"]
}
```

### M2.3 偏差

- **没用 gix 算 diff**：直接 shell-out `git`。理由：gix 的 working-tree-vs-tree diff 高级 API 不齐，自己拼太重。等 M3+ 性能成瓶颈再考虑切。
- **No subscription / `notify` watch**：M2.3 只做一次性 `Diff.Get`。`File.Watch` / `Diff.Subscribe` 等 push 通道留给 M3 的 agent timeline 一起做（subscription 帧需要 sub_id 字段，到时 wire 协议加一轮）。
- **No binary read/write**：UTF-8 only，碰到二进制文件返清晰错误。binary attachment plumbing 在 M2.4 用 PTY 输出时一起加。
- **No gitignore filter**：`tree` 不读 `.gitignore`，会列出 `target/` `node_modules/` 这种。等 frontend 真碰到性能问题再考虑用 `git ls-files --cached --others --exclude-standard` 替代。

### M2.1 端到端实测（2026-04-25）

```bash
# 准备一个空 repo 当 project_root
git init -b main /tmp/charon-m21-test-project
git -C /tmp/charon-m21-test-project commit --allow-empty -m init

# 经 NyxID proxy 创建
curl -X POST -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/json" \
  -d '{"project_root":"/tmp/charon-m21-test-project","title":"M2.1 smoke"}' \
  https://nyx-api.chrono-ai.fun/api/v1/proxy/s/charon-echo-poc/api/v1/workspaces
```

返回（HTTP 201）：
```json
{
  "id": "ea90f371-bd08-4b13-b25d-4ef2156b9c68",
  "title": "M2.1 smoke",
  "project_root": "/private/tmp/charon-m21-test-project",
  "base_branch": "main",
  "branch": "charon/ea90f371",
  "worktree_path": "/Users/chronoai/.charon/worktrees/ea90f371-bd08-4b13-b25d-4ef2156b9c68",
  "created_at": "2026-04-25T15:19:39.195366Z"
}
```

硬盘上 `~/.charon/worktrees/<id>/.git` 是 git worktree 标记文件，`git rev-parse --abbrev-ref HEAD` 在该目录返回 `charon/ea90f371`。`~/.charon/workspaces.json` pretty-printed 落盘。

错误路径：
- 无 JWT → 401 `missing_identity_token`
- 路径不是 git repo (`/etc`) → 400 `create_failed`，message 透传 `git rev-parse` stderr
- 相对路径 (`./x`) → 400 `create_failed: project_root must be an absolute path`
- archive 不存在 id → 404 `archive_failed: workspace ... not found`
- get 不存在 id → 404 `not_found`
