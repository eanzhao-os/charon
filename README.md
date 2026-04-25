# Charon

> 一个跑在你 dev 机器上的 Rust daemon，把这台机器变成"任何设备都能远程驱动的 AI coding 工位"。代码物理上始终在你自己的机器，远程接入这件事白嫖 [NyxID](https://github.com/ChronoAIProject/NyxID) 的反向隧道。

## 名字

Charon（卡戎）是希腊神话冥河船夫，在两界之间摆渡。这正是这个 daemon 在做的事——把你的 dev 环境从那台高规格工作站摆渡到你手边的任何设备：通勤路上的手机、咖啡厅的 MacBook、家里的 iPad。

在多种神谱里 Charon 是 Nyx 与 Erebus 之子，跟 NyxID 一脉相承。

## 这个 repo 在做什么

```
┌────────────────────────────┐
│ 任何设备                    │  iPhone / iPad / 别人的 Mac / 自己的 Mac
└─────────────┬──────────────┘
              │ HTTPS / WSS, NyxID JWT
              ▼
┌────────────────────────────┐
│      NyxID Backend         │  反向隧道 + auth + audit + approval
└─────────────┬──────────────┘
              │ NyxID node WS frame protocol
              ▼
┌────────────────────────────┐
│    nyxid node agent        │  跑在你 dev 机上
└─────────────┬──────────────┘
              │ HTTP/WS to localhost:18789
              ▼
┌────────────────────────────┐
│    charon-daemon (本仓库)   │
│    workspace / agent /     │
│    terminal / file / diff   │
│              ↓ spawn        │
│   claude / codex / opencode │
└────────────────────────────┘
```

Charon daemon 在 dev 机上：

- 用 git worktree 隔离 workspace
- 托管多种 AI coding agent（Claude Code / Codex / OpenCode），统一接口
- 暴露文件 / diff / PTY terminal API 给客户端
- 接收 client 经 NyxID 反向隧道过来的连接，绝不直接对外

它**不是**：托管服务（你自带算力）、云 IDE（代码不上你机器以外）、NyxID 的 fork（独立项目，零 NyxID 代码改动）。

## 为什么 NyxID

[NyxID](https://github.com/ChronoAIProject/NyxID) 已经把 dev 工具最难做的几件事做掉了：反向隧道穿 NAT、JWT 鉴权、audit log、approval 审批 API、credential broker、移动推送。Charon 把自己注册成 NyxID 用户的一个 `UserService`，直接借这套基础设施，专心做 workspace runtime 这一层。

| NyxID 给的 | Charon 自己做的 |
|---|---|
| 反向隧道、TLS、JWT、audit | git worktree、agent runtime、PTY、diff |
| 凭证 broker（拉 ANTHROPIC_API_KEY 等） | spawn agent 进程、注入环境 |
| 手机推送 + approval API | 拦截危险工具调用、回头调审批 |
| 复用同一套 React/TanStack/Tailwind 组件库 | Tauri desktop 壳子 |

**完全不需要修改 NyxID 任何代码**——这是经过逐步代码核实的结论，详见 [`docs/02-nyxid-integration.md`](docs/02-nyxid-integration.md)。

## 愿景

> Charon = paseo 的本地能力 × NyxID 的远程通路

把"必须在 dev 机上才能用"这个限制用 NyxID 反向隧道解掉，让一个高规格工作站同时服务你的所有设备：

- 通勤路上在手机点几下，触发后台 Claude 开始写一个 PR
- 在咖啡厅打开 MacBook，无缝接管那个 PR，看 timeline 和 diff
- 回家用平板 review，审批通过让 agent 自己 commit + push

代码、构建、agent 进程、本地工具——全部跑在那台你信任的、跑得动的机器上。手边的设备只负责显示和操作。

## 技术栈

| 层 | 选型 |
|---|---|
| Daemon 语言 | Rust 1.93+（edition 2024，resolver 3） |
| HTTP/WS server | [axum](https://github.com/tokio-rs/axum) 0.8 |
| 异步 runtime | [tokio](https://tokio.rs) |
| JWT 验签 | [jsonwebtoken](https://github.com/Keats/jsonwebtoken) v9（RS256 + 自动从 NyxID JWKS 拉 key） |
| HTTP client | [reqwest](https://github.com/seanmonstar/reqwest)（rustls） |
| CLI | [clap](https://docs.rs/clap) v4 derive |
| 日志 | [tracing](https://github.com/tokio-rs/tracing) + tracing-subscriber |
| Git 操作（M2+） | [gix](https://github.com/Byron/gitoxide) |
| PTY（M2+） | [pty-process](https://crates.io/crates/pty-process) |
| Desktop 客户端（M2+） | [Tauri](https://tauri.app) v2 + React 19 + TanStack + Tailwind 4 |
| MCP loopback（M3+） | [rmcp](https://crates.io/crates/rmcp) |

跟 NyxID 同栈（axum / tokio / reqwest / tracing / Tauri）是故意的——共享心智模型，desktop 还能直接复用 NyxID frontend 的组件库。

## 当前状态

| 里程碑 | 范围 | 状态 |
|---|---|---|
| M1 Hello World | daemon + axum + JWT 验签 + CLI doctor + CI | ✓ 已完成 |
| M2 真实 UX | git worktree、文件/diff/terminal、Tauri desktop client | 进行中 |
| M3 多 agent + MCP 回环 | provider registry、MCP loopback、approval bridge | 待开始 |
| M4 移动 / schedule | 移动 UI、scheduled agent、多 workspace 并发 | 待开始 |

M1 已经能：

- 在 `127.0.0.1:18789` 起 axum daemon
- `GET /api/v1/health` 公开返回版本
- `GET /api/v1/whoami` 强制校验 `X-NyxID-Identity-Token`（RS256，NyxID JWKS 验签，30s leeway，aud + iss 都对得上才放行）
- `charon doctor` 三段自检：本地 daemon → nyxid node daemon → 经 NyxID proxy 端到端 `/whoami`

详见 [`docs/05-poc-results-and-next-steps.md`](docs/05-poc-results-and-next-steps.md) 的"M1 实施进度"。

## 快速开始

前提：

- Rust 1.93+
- 一个 NyxID 账号 + 已注册的 nyxid node（参考 [`docs/02-nyxid-integration.md`](docs/02-nyxid-integration.md) 的"一次配置流程"）
- 一个指向 `http://localhost:18789` 的 NyxID UserService（M1 阶段直接复用 PoC 的 `charon-echo-poc` slug）

```bash
# 1. 编
cargo build --release --workspace

# 2. 起 daemon（绑 127.0.0.1:18789）
./target/release/charon-daemon
# 或 cargo run -p charon-daemon

# 3. 自检
./target/release/charon doctor
```

期望输出：

```
== charon doctor ==

[1] local charon-daemon at http://127.0.0.1:18789
  ✓ charon-daemon 0.0.1
[2] nyxid node daemon
  ✓ running (PID 69251)
[3] end-to-end via NyxID proxy
    GET https://nyx-api.chrono-ai.fun/api/v1/proxy/s/charon-echo-poc/api/v1/whoami
  ✓ user_id=… email=…
All checks passed.
```

环境变量（都有 sensible default）：`CHARON_BIND` / `CHARON_EXPECTED_AUD` / `CHARON_NYXID_ISSUER` / `CHARON_ENDPOINT` / `CHARON_NYXID_BASE_URL` / `CHARON_DOCTOR_SLUG`。完整列表见 [`docs/05`](docs/05-poc-results-and-next-steps.md#env-控制点速查)。

## 仓库布局

```
charon/
├── crates/
│   ├── charon-core/     # 跨 crate 共享的 wire 类型 + 常量
│   ├── charon-daemon/   # axum HTTP/WS server，NyxID JWT 验签
│   └── charon-cli/      # `charon` 命令行
├── poc/echo-daemon/     # M1 之前用来验证全链路的 PoC echo daemon（保留作回归 smoke）
├── docs/                # 设计文档（按 01 → 05 顺序读）
└── .github/workflows/   # CI
```

## 文档

按编号顺序读：

1. [`01-overview.md`](docs/01-overview.md) — 项目定位 + 与 paseo 的对比
2. [`02-nyxid-integration.md`](docs/02-nyxid-integration.md) — NyxID 集成、配置流程、硬约束
3. [`03-architecture.md`](docs/03-architecture.md) — 系统架构 + 组件分解
4. [`04-tech-stack.md`](docs/04-tech-stack.md) — 完整技术栈选型理由
5. [`05-poc-results-and-next-steps.md`](docs/05-poc-results-and-next-steps.md) — PoC 结果 + M1 进度日志

## License

Apache-2.0
