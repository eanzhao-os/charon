# Charon — Overview

Charon 是一个跑在你 dev 机器上的 daemon，把"远程 AI coding workspace"做成本地资源，但允许从任意设备访问。它的角色对标 paseo 的本地 daemon，**关键差别在于通过 NyxID 的反向隧道把"任意网络下的客户端连接"这件事白送过来**。

## 名字

Charon（卡戎）是希腊神话中冥河船夫，在两界之间摆渡。这正是这个 daemon 在做的事：把你的 dev 环境从那台高规格机器摆渡到你手边的任何设备——咖啡厅的 MacBook、地铁上的手机、家里的 iPad。

在多种神谱里，Charon 是 Nyx 与 Erebus 之子，跟 NyxID 一脉相承。

## 它是什么

- 一个 Rust daemon，跑在 Linux/macOS dev 机器上（可 headless）
- 用 git worktree 隔离 workspace
- 托管多种 AI coding agent：Claude Code、Codex、OpenCode 等，统一接口
- 暴露文件读写、diff、PTY terminal API 给客户端
- 接收 client 经 NyxID 反向隧道过来的连接

## 它不是什么

- **不是托管服务**——你自带计算资源，daemon 跑在你自己的机器
- **不是云 IDE**——不在浏览器里编译运行你的代码，代码物理上一直在你机器上
- **不是 NyxID 的 fork**——是独立项目，零 NyxID 代码改动

## 与 paseo 的对比

| | paseo | Charon |
|---|---|---|
| 架构 | 本地 daemon + 本地客户端 | 本地 daemon + 经 NyxID 路由的远程客户端 |
| 网络模型 | 仅本机/同 LAN | 任意网络，反向隧道穿 NAT |
| Auth | 无（本地信任） | NyxID JWT 全程鉴权 |
| 审计 | 无 | NyxID audit logs |
| 多设备 | 单机 | 桌面/手机/平板任意切换 |
| 凭证管理 | 本地配置 | NyxID credential broker |
| 工程量 | 大（auth/transport 全自己做） | 小（NyxID 接管 transport+auth） |
| 实现语言 | TypeScript / Node.js | **Rust** |

## 一句话定位

> **Charon = paseo 的本地能力 × NyxID 的远程通路**

把 paseo 把"workspace、agent runtime、文件、diff、terminal"封装成本地 daemon 的工程价值原样保留，把"必须本地连"这个限制用 NyxID 的反向 WS 隧道解掉。
