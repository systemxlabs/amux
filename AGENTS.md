# AGENTS.md

## 项目概览

amux 是一个 **agent 控制平面**：GUI 桌面应用，统一调度多台机器上的 agent（Codex、Claude Code、Kimi Code），采用 Client-Server 架构，**全 Rust 实现**（GPUI 桌面应用 + Rust server + ACP v1）。产品定位见 [docs/PRD.md](docs/PRD.md)，系统架构见 [docs/DESIGN.md](docs/DESIGN.md)。

## 项目状态

amux 处于 **beta 阶段**：允许随意破坏性改动，无需向后兼容，不为旧数据、旧格式、旧行为写迁移或兜底。

## 文档与实现

- [docs/PRD.md](docs/PRD.md)（产品需求）、[docs/DESIGN.md](docs/DESIGN.md)（系统设计）是**框架文档**，项目实现必须遵循整个产品和技术框架，细节部分可自行决策
- 实现与框架文档冲突时：不要擅自偏离文档，**交由人来决策**
- 实现过程中主动**判断框架文档是否需要完善**——发现未覆盖、表述不清或已过时的决策时，提出修订建议

## 仓库结构（Rust workspace）

| crate | 职责 |
|---|---|
| `protocol` | app↔server 协议面：方法名、参数/结果类型、通知类型。**协议的唯一来源**，GUI 与 server 均从这里导入 |
| `server` | 每台机器的常驻进程：WebSocket 传输、JSON-RPC 分发、会话管理、会话数据聚合与 activities 缓存、git 能力；经 ACP 与 agent 通信 |
| `gui` | GPUI 桌面应用 |

关键边界：

- server 与 agent 的交互**只经 ACP 协议**
- server 之间不通信；跨机器编排在 GUI 侧完成

## 工程原则

- 尽可能复用已有的库，不要重复造轮子，仅在现有库确实无法满足需求时才自己实现，并说明理由
- 遵循 Rust 和软件工程最佳实践
- 尽量使用强类型