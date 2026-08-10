# AGENTS.md

## 项目概览

amux 是一个 **agent 控制平面**（beta 阶段，个人工具）：GUI 桌面应用，统一调度多台机器上的 agent（Codex、Claude Code、Kimi Code），采用 Client-Server 架构，**全 Rust 实现**（GPUI 桌面应用 + Rust server + ACP v1）。产品定位见 [docs/PRD.md](docs/PRD.md)，系统架构见 [docs/DESIGN.md](docs/DESIGN.md)。

## 项目状态

amux 处于 **beta 阶段**：允许随意破坏性改动，无需向后兼容，不为旧数据、旧格式、旧行为写迁移或兜底。

## 文档与实现

- [docs/PRD.md](docs/PRD.md)（产品需求）、[docs/DESIGN.md](docs/DESIGN.md)（系统设计）是**框架文档**，项目实现必须遵循；详略约定见各文档开头
- **PRD 变更即触发代码同步**——不要以"旧实现/已做决定"为由拒绝跟进 PRD 的修改
- 实现与框架文档冲突时：不要擅自偏离文档，**交由人来决策**
- 实现过程中主动**判断框架文档是否需要完善**——发现未覆盖、表述不清或已过时的决策时，提出修订建议

## 仓库结构（Rust workspace）

| crate | 职责 |
|---|---|
| `protocol` | app↔server 协议面：方法名、参数/结果类型、通知类型。**协议的唯一来源**，GUI 与 server 均从这里导入 |
| `server` | 每台机器的常驻进程：WebSocket 传输（tokio-tungstenite）、JSON-RPC 分发、会话管理、会话数据聚合与 activities 缓存、git 能力（status/diff/push/revert）；经 ACP 官方 SDK（`agent-client-protocol`）与 agent 通信。入口 `server/src/main.rs` |
| `gui` | GPUI 桌面应用：三面板视图（Dock 布局）、对话流（Markdown）、会话活动、diff 编辑器、侧边栏、设置页；会话历史本地缓存 |

关键边界：

- server 与 agent 的交互**只经 ACP 协议**（官方 SDK `agent-client-protocol` + `agent-client-protocol-tokio`，docs/DESIGN.md §9）
- server 之间不通信；跨机器编排在 GUI 侧完成

## 技术栈与约定

- 全仓库 **Rust**（cargo workspace），统一 `rustfmt` + `clippy`（`-D warnings`）
- 异步运行时 **tokio**；JSON-RPC 用 `serde`/`serde_json`；WebSocket 用 `tokio-tungstenite`
- GUI 用 **gpui + gpui-component**（跟踪 Zed 主线 git 依赖，见 docs/DESIGN.md §7）
- 协议类型集中在 `protocol` crate，GUI 与 server 从同一处导入，不各自定义
- 代码注释、文档使用**中文**；新代码沿用这一习惯

## 构建与测试命令

```bash
cargo build                      # 构建全部 crate
cargo test                       # 全部 crate 测试
cargo clippy -- -D warnings
cargo fmt --check
```

单 crate 操作：

```bash
cargo run -p server -- --token <值>   # 启动 server 常驻进程
cargo test -p server                  # 仅 server 测试
cargo run -p gui                      # 启动 GPUI 桌面应用
```

## 测试约定

- 测试框架统一为 **cargo test**（内置）
- 测试文件位置跟随 Rust 惯例：
  - 单元测试：源码同文件 `#[cfg(test)] mod tests`
  - 集成测试：各 crate 的 `tests/` 目录
  - `protocol`：类型 / JSON-RPC 编解码测试；`server`：会话管理 / 事件路由 / ACP 对接 / 传输测试；`gui`：纯逻辑（协议状态、历史缓存）测试，UI 组件无测试
- 新增功能时应补测试（项目已有测试覆盖），提交前跑 `cargo test && cargo clippy -- -D warnings && cargo fmt --check`

## 工程原则

- 尽可能**复用已有的库**，不要重复造轮子：优先使用成熟、维护中的库（tokio、serde、tokio-tungstenite、gpui、gpui-component），以及仓库内已有的 workspace crate；仅在现有库确实无法满足需求时才自己实现，并说明理由
- 最小改动：bug 修复不附带清理，简单功能不加多余的配置项
- 协议改动只动 `protocol` crate 一处，GUI 与 server 跟随更新
