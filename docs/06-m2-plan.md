# M2 — 真实 UX

> 跟 docs/05 同样体例：上半部分是计划+决策，下半部分是 live progress log。

## 目标

把 M1 的 health/whoami 骨架长成"客户端能真的开 workspace、写文件、看 diff、跑 terminal"的最小可用形态。完成后桌面客户端能从手机/咖啡厅 MacBook 经 NyxID proxy 操作一台远端 dev 机的代码。

## 分块（按依赖顺序）

| 子里程碑 | 范围 | 依赖 | 状态 |
|---|---|---|---|
| **M2.1 Workspace 模型** | git worktree manager（shell-out 到 `git`）、`~/.charon/workspaces.json` 持久化、`POST/GET /api/v1/workspaces`、`POST /api/v1/workspaces/:id/archive`，全部走 `IdentityToken` | M1 | ✅ 2026-04-25 |
| **M2.2 WS 协议** | `GET /api/v1/ws` 升级；JSON envelope + 36B UUID 前缀 binary attachment（参考 NyxID node frame）；帧命名空间 `Workspace.* / File.* / Diff.* / Terminal.*`；60s ping 避开 NyxID 300s idle | M2.1 | ⏳ |
| **M2.3 File / Diff API** | 路径校验严格在 worktree 内（`canonicalize` 前缀 check）；`gix` 算 working-tree vs base diff，按文件分块；`notify` 监听变化；大文件 (>1MB) 分片帧 | M2.2 | ⏳ |
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
