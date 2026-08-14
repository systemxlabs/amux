# AGENTS.md

## 项目概览

amux 是一个 **agent 控制平面**：GUI 应用，统一调度多台机器上的 agent（Codex、Claude Code、Kimi Code），采用 Client-Server 架构，**全 Rust 实现**（GUI 应用 + Rust server + ACP v1）。产品定位见 [docs/PRD.md](docs/PRD.md)，系统架构见 [docs/DESIGN.md](docs/DESIGN.md)。

## 项目状态

amux 处于 **beta 阶段**：允许随意破坏性改动，无需向后兼容，不为旧数据、旧格式、旧行为写迁移或兜底。

## 文档与实现

- [docs/PRD.md](docs/PRD.md)（产品需求）、[docs/DESIGN.md](docs/DESIGN.md)（系统设计）是**框架文档**，项目实现必须遵循整个产品和技术框架，细节部分可自行决策
- 实现与框架文档冲突时：不要擅自偏离文档，**交由人来决策**
- 实现过程中主动**判断框架文档是否需要完善**——发现未覆盖、表述不清或已过时的决策时，提出修订建议
- **文档改动审核门禁**：涉及任何文档（`docs/PRD.md`、`docs/DESIGN.md` 及仓库内其他 `.md` 文档，含 `AGENTS.md` 自身）的修改，**必须先展示实际 diff 并经用户明确确认，才能提交（commit）与推送（push）**。用户对「改动方向」的口头/文字同意**不等于**对具体 diff 的确认——必须把实际改动内容呈现给用户、得到明确确认后再提交。文档改动应与代码改动分开处理：代码改动可正常提交；文档改动单独呈现，确认后再提交/推送

## 仓库结构

| crate | 职责 |
|---|---|
| `protocol` | GUI 应用 ↔ server 协议面：方法名、参数/结果类型、通知类型。**协议的唯一来源**，GUI 应用与 server 均从这里导入 |
| `server` | 每台机器的常驻进程：WebSocket 传输、JSON-RPC 分发、会话管理、ACP 会话事件透传、git 能力；经 ACP 与 agent 通信 |
| `gui` | GUI 应用（会话事件聚合：对话流与活动） |

## 工程原则

- 尽可能复用已有的库，不要重复造轮子，仅在现有库确实无法满足需求时才自己实现，并说明理由
- 遵循 Rust 和软件工程最佳实践
- 尽量使用强类型