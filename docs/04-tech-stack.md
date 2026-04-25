# Charon 技术栈

主语言：**Rust**（daemon、CLI、共享 model、Tauri 后端）
桌面前端：TypeScript + React（在 Tauri 壳子内）

理由：跟 NyxID 同栈（axum 0.8、tokio、reqwest、tracing），共享心智模型；agent 进程管理 / PTY / 跨平台守护进程在 Rust 里成熟；Tauri 让我们用 web 生态做 paseo-quality UI 而不必跟 native widget 框架搏斗。

## Daemon

| 用途 | crate | 备注 |
|---|---|---|
| HTTP + WS server | `axum` 0.8 | 跟 NyxID 同版本同栈 |
| 异步 runtime | `tokio` | |
| WS server | `axum` built-in upgrade + `tokio-tungstenite` | binary frame 双向 |
| PTY | `pty-process` | NyxID CLI 已在用，参考 `cli/src/node/ws_client.rs` |
| Git 操作 | `gix` (gitoxide) | 纯 Rust，无 libgit2 依赖；功能不全的边缘情况降级到 `git2` |
| 文件 watch | `notify` | 跨平台 inotify / FSEvents / ReadDirectoryChangesW |
| Serde | `serde` + `serde_json` | |
| JWT 验证 | `jsonwebtoken` | 用 NyxID JWKS（RS256） |
| 调 NyxID API | `reqwest` (rustls) | |
| MCP server 实现 | `rmcp`，不行就基于 `axum` 自实现 MCP HTTP transport | |
| CLI | `clap` v4 derive | |
| 配置路径 | `directories` | 跨平台 user dir |
| 日志 | `tracing` + `tracing-subscriber` | 结构化 |
| 错误处理 | `anyhow`（lib boundary）+ `thiserror`（内部 enum） | 跟 NyxID 一致 |
| 守护进程化 | 手写 launchd plist + systemd unit 模板 | 模式参考 `nyxid/cli/src/node/daemon.rs` |
| 本地敏感数据 | `aes-gcm` + `argon2` 或 OS keychain (`keyring`) | 跟 NyxID node `secret_backend.rs` 对齐 |
| 取消 / shutdown | `tokio_util::sync::CancellationToken` | |
| HTTP retries | `reqwest-retry` 或自写 backoff | |
| Stream | `tokio-stream` + `futures` | |
| Time / DateTime | `chrono` | |

## Desktop 客户端

走 **Tauri v2**：Rust 后端 + web 前端。

理由：
- 复杂 UI（agent timeline、code editor、diff viewer、PTY 渲染）web 栈生态成熟太多
- NyxID frontend 已经是 React 19 + TypeScript + Tailwind 4 + TanStack Router/Query，**Charon Desktop 直接复用同一套组件库 + 设计语言**，零认知负担
- Tauri 后端（Rust）跟 daemon 共享 model 定义（`charon-core` crate 直接 `pub use`），零类型偏移

| 用途 | 选型 |
|---|---|
| Shell + Rust 后端 | `tauri` 2.x |
| 前端 | TypeScript + React 19 + Vite |
| 路由 / 数据层 | TanStack Router + Query（同 NyxID） |
| 样式 | Tailwind 4（同 NyxID） |
| Code editor | Monaco（diff editor 自带）或 CodeMirror 6 |
| Terminal 渲染 | `xterm.js` |
| Tauri 后端 WS | `tokio-tungstenite` |
| 状态 | Zustand（同 NyxID） |
| 表单 / 校验 | React Hook Form + Zod（同 NyxID） |

### 纯 Rust UI 备选方案

如果偏好不带 web 层：
- **`dioxus`** — Rust 写 React-like UI，desktop 模式可用，社区在长大
- **`iced`** — pure Rust native widget，Elm-like 架构
- **`egui`** — immediate mode，适合工具类 UI 但不适合 IDE 风格的 UI

考虑到 paseo 的 UI 复杂度（timeline 流式动画、Monaco-级别编辑器、diff、PTY 渲染）和上市时间，**首选 Tauri**。如果产品定位是"轻量工具"而非"全功能 IDE-like 客户端"，再考虑 dioxus / iced。

## 测试

| 用途 | crate |
|---|---|
| Unit / integration test | `tokio::test` + `assert_matches` |
| HTTP mock | `mockito` 或 `wiremock` |
| End-to-end | 起真 daemon + 真 NyxID 沙箱（dev compose） |
| Fixture 数据 | `insta` snapshot（跟 NyxID 一致） |
| Property test（关键协议） | `proptest` |

## CLI 子命令初步规划

```
charon daemon install / start / stop / restart / status / logs / uninstall
charon link                  # 一键完成 NyxID node 注册 + UserService 创建
charon login                 # 走 NyxID OAuth，本地缓存 JWT（用于 charon CLI 自身调 NyxID API）
charon doctor                # 自检：daemon up? node connected? UserService 在?

charon ws    create / list / archive / show
charon agent spawn / list / kill / send / wait / show
charon term  create / send / show / kill
```

CLI 主要给运维和脚本用；交互式工作以 desktop 为主。

## 仓库布局

```
charon/
├── Cargo.toml              # workspace
├── crates/
│   ├── charon-core/        # workspace / agent / terminal / file / diff 模型 + trait
│   ├── charon-daemon/      # 长进程，axum server，组装 core 模块
│   ├── charon-cli/         # `charon` CLI
│   ├── charon-mcp/         # MCP loopback server
│   ├── charon-nyxid/       # NyxID API client：JWT 验证、approval 桥、credential broker 客户端
│   └── charon-desktop/     # Tauri app（src-tauri 是 Rust，src 是 TS+React）
├── docs/                   # 本目录
├── scripts/                # 构建 / 打包 / 发布
└── deploy/                 # launchd plist / systemd unit 模板
```

## 与 NyxID 共享的代码

我们不会把 NyxID 当 dependency 拉进来——耦合度太高。但以下模式直接照搬：

- **错误层**：`AppError` enum + `AppResult<T>` 模式（NyxID `errors/mod.rs`）
- **配置**：`AppConfig` from env vars 模式（NyxID `config.rs`）
- **日志**：`tracing` + JSON formatter 配置
- **模型 serde**：UUID v4 string + chrono datetime 的处理（参考 NyxID `models/bson_datetime.rs`）
- **Daemon install**：launchd / systemd 模板（参考 NyxID `cli/src/node/daemon.rs`）

## 不引入的东西

- **不要 MongoDB**：Charon 的状态全都在本地 JSON 文件 + workspace 内的 git 状态里，引入数据库违反"本地优先、零远端状态"原则
- **不要自建 OAuth provider**：复用 NyxID
- **不要自建审计**：写到 NyxID audit log
- **不要 Docker 默认依赖**：daemon 必须能直接 `cargo install` 跑起来，docker 是可选打包形态
