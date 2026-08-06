# AGENTS.md

## 项目概览

amux 是一个 **agent 控制平面**（beta 阶段，个人工具）：GUI 桌面应用，统一调度多台机器上的 agent harness（Codex、Claude Code、Kimi Code），采用 Client-Server 架构。产品定位见 [docs/PRD.md](docs/PRD.md)，系统架构见 [docs/DESIGN.md](docs/DESIGN.md)。

## 项目状态

amux 处于 **beta 阶段**：允许随意破坏性改动，无需向后兼容，不为旧数据、旧格式、旧行为写迁移或兜底。

## 文档与实现

- [docs/PRD.md](docs/PRD.md)（产品需求）、[docs/DESIGN.md](docs/DESIGN.md)（系统设计）、[docs/AHAL.md](docs/AHAL.md)（harness 接口）是**框架文档**，项目实现必须遵循；详略约定见各文档开头
- **PRD 变更即触发代码同步**——不要以"旧实现/已做决定"为由拒绝跟进 PRD 的修改
- 实现与框架文档冲突时：不要擅自偏离文档，**交由人来决策**
- 实现过程中主动**判断框架文档是否需要完善**——发现未覆盖、表述不清或已过时的决策时，提出修订建议

## 仓库结构（pnpm monorepo）

| 包 | 职责 |
|---|---|
| `ahal` | Agent Harness Access Layer 核心：纯类型与接口（`Driver`/`Session`/事件流），**零依赖、无运行时**。语义依据 docs/AHAL.md |
| `ahal-codex` | Codex driver：spawn `codex app-server` 子进程（JSON-RPC over stdio） |
| `ahal-claude` | Claude driver：进程内封装 `@anthropic-ai/claude-agent-sdk` |
| `ahal-kimi` | Kimi driver：进程内封装 `@botiverse/kimi-code-sdk` |
| `shared` | app↔server 协议面：方法名、参数/结果类型、通知类型。**协议的唯一来源**，server 与 app 均从这里导入 |
| `server` | 每台机器的常驻进程：WebSocket 传输（`ws`）、JSON-RPC、会话管理、事件持久化、git 能力。入口 `src/index.ts` |
| `app` | Tauri 2 桌面 GUI：React 19 + Vite 7。`src/lib/` 为状态与逻辑（store、通知、视图模型），`src/ui/` 为组件；`src-tauri/` 为 Rust 壳（目前基本是模板） |

## 技术栈与约定

- 全仓库 **TypeScript ESM**（各包 `"type": "module"`），strict 模式，统一继承 `tsconfig.base.json`（`noEmit`，无编译产物）
- workspace 包直接以 `"exports": "./src/index.ts"` 暴露 **TS 源码**，无构建步骤——server 用 `tsx` 直接跑，app 由 Vite 直接消费
- 相对导入写 `.js` 扩展名（如 `import { x } from "./foo.js"`），这是 ESM 惯例，新增文件保持一致
- 代码注释、文档使用**中文**；新代码沿用这一习惯

## 构建与测试命令

```bash
pnpm install          # 安装依赖（pnpm workspace）
pnpm typecheck        # 根目录：对所有包执行 tsc --noEmit
pnpm test             # 根目录：对所有包执行 vitest run
```

单包操作（在包目录下或 `pnpm --filter <包名> <命令>`）：

```bash
cd server && pnpm dev          # tsx 启动 server 常驻进程
cd server && pnpm test         # 仅跑 server 测试
cd app && pnpm dev             # vite 起前端（端口 1420）
cd app && pnpm tauri dev       # 起完整桌面应用（需 Rust 工具链）
cd app && pnpm build           # tsc && vite build
```

## 测试约定

- 测试框架统一为 **vitest**（根 devDependency，各包共用），命令 `vitest run`
- 测试文件位置各包不同，跟随所在包的惯例：
  - `server`：与源码同目录，`src/*.test.ts`
  - `app`：`src/lib/*.test.ts`（UI 组件无测试）
  - `ahal-*`：独立的 `test/` 目录，主要测 `normalize.ts` 事件归一化
  - `ahal`：`test/types.test.ts`（类型级测试）
- 新增功能时应补测试（项目已有测试覆盖），提交前跑 `pnpm typecheck && pnpm test`

## 工程原则

- 尽可能**复用已有的库**，不要重复造轮子：优先使用成熟、维护中的库（如 `ws`、`tsx`、`vitest`、Tauri 生态包），以及仓库内已有的 workspace 包；仅在现有库确实无法满足需求时才自己实现，并说明理由
- 最小改动：bug 修复不附带清理，简单功能不加多余的配置项
- 协议改动只动 `shared/src/protocol.ts` 一处，server 与 app 跟随更新
