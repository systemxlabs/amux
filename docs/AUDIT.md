# 代码审计记录（按 AGENTS.md 工程原则）

审计日期：2026-08-11
范围：全 workspace 三个 crate（protocol / server / gui）与 workspace 配置。
依据：AGENTS.md 三条工程原则——
1. 尽可能复用已有的库，不要重复造轮子，仅在现有库确实无法满足需求时才自己实现，并说明理由
2. 遵循 Rust 和软件工程最佳实践
3. 尽量使用强类型

判定口径：`fixed` = 本轮已修复；`justified` = 保留并给出具体理由（AGENTS.md 允许「自己实现并说明理由」）；`pass` = 审计未发现该原则下的问题。

---

## 一、复用已有库（不重复造轮子）

### protocol crate

| # | 位置 | 发现 | resolution |
|---|------|------|-----------|
| P1 | `src/jsonrpc.rs` | 自实现 JSON-RPC 2.0 信封类型（Request/Notification/Response/Error）与错误码 | **justified**：DESIGN §1/§2/§4「协议唯一来源」——app↔server 协议面必须在共享 crate 定义，GUI 与 server 从同一处导入；引入 jsonrpsee 等会把协议定义移出单一来源 crate，违背框架文档。信封字段与标准错误码均为薄定义，无重复实现逻辑 |
| P2 | `src/log.rs` | 自实现极简日志器（`AMUX_LOG` 控级别） | **justified**：DESIGN §8 明确「具体格式、级别策略、覆盖范围与落盘位置交由实现 agent 决定」；本实现零额外依赖（protocol crate 保持零依赖），tracing/log 引入会改变协议 crate 依赖面并重定义日志格式 |
| P3 | `src/types.rs` / `src/methods.rs` | 协议数据类型与方法/通知名常量 | **pass**：typed 结构体 + 常量集中定义，即「协议唯一来源」本身，无重复实现 |

### server crate

| # | 位置 | 发现 | resolution |
|---|------|------|-----------|
| S1 | `src/session.rs` | 手写 `uuid_v4()`（nanos+pid 拼 24 hex）生成会话 id | **fixed**：改为 `uuid::Uuid::new_v4()`（uuid 1.24 已在依赖树，v4 无新增传递依赖），手写函数删除 |
| S2 | `src/git.rs` | 手写临时目录命名（pid+nanos）用于 revert patch | **fixed**：改为 uuid v4（`amux-revert-<uuid>`），生产 revert 与测试 helper 均替换 |
| S3 | `src/git.rs` | git 能力经 CLI 子进程（status/diff/push/revert）而非 git2 crate | **justified**：DESIGN §6「直连 git」未指定实现；git2 引入 libgit2 原生依赖与构建复杂度，CLI 方式零依赖且覆盖所需子集，失败语义（非仓库/无变更）与命令行行为一致 |
| S4 | `src/transport.rs` | WS 上 JSON-RPC 帧收发 | **justified**：信封类型由 protocol crate 统一（P1），此处为薄层（tungstenite 传输 + 序列化/反序列化 + 按 id 匹配），无现成轻量「WS+JSON-RPC 服务端分发」crate 可直接替换且不破坏协议单一来源 |
| S5 | `src/agent.rs` | ACP `skill/list` SDK schema 未收录 → 自定义请求类型 | **justified**：使用官方 SDK 的 derive 宏（`JsonRpcRequest`）按 SDK 扩展点自建，这是官方 SDK 支持的自定义方法方式；响应已 typed 化（见 T3） |
| S6 | `src/{session,git}.rs` 等 | 手写 `now()`（毫秒时间戳） | **justified**：4 行琐碎 helper（`SystemTime::duration_since`），为毫秒时间戳引入 chrono 等时间库属过度依赖 |

### gui crate

| # | 位置 | 发现 | resolution |
|---|------|------|-----------|
| G1 | `src/workflow.rs` | 手写 `uuid_v4()`（nanos+pid）生成编排会话 id | **fixed**：改为 `uuid::Uuid::new_v4()`，手写函数删除 |
| G2 | `src/ws.rs` | WS 上 JSON-RPC 客户端（请求/响应按 id 匹配、通知广播） | **justified**：信封类型由 protocol crate 统一（P1）；此处为薄层客户端（tungstenite + 通道 + id 匹配），与 server 端分发语义不同（客户端请求/通知、服务端分发），共享抽象已在 protocol |
| G3 | `src/logic.rs` | `compose_prompt` / `parse_at_references` / `read_path_context` 等 | **justified**：amux 领域逻辑（@ 引用解析、附件→ACP ContentBlock 映射），无现成 crate 覆盖，且为产品语义 |
| G4 | `src/workflow.rs` RigBackend | 每次 `decide()` 重建 rig OpenAI client | **justified**：rig client 为无状态配置构建（成本可忽略），LLM 调用本身是主成本；缓存需引入 Mutex 增加复杂度，收益不成比例 |

---

## 二、遵循 Rust 与软件工程最佳实践

### protocol crate

| # | 位置 | 发现 | resolution |
|---|------|------|-----------|
| P4 | `src/{jsonrpc,types}.rs` 测试 | 测试内 `unwrap()`/`panic!` | **justified**：测试代码断言失败即 panic 属 Rust 测试惯例，非生产路径 |

### server crate

| # | 位置 | 发现 | resolution |
|---|------|------|-----------|
| S7 | workspace `Cargo.toml` | `rust-version = "1.80"` 低于 agent-client-protocol 1.3.0 要求（1.88） | **fixed**：升至 `1.88`（官方 SDK 的 rust-version 要求），本机 1.98 不受影响，旧 CI 不再失败 |
| S8 | `src/{agent,session,transport}.rs` 生产代码 | 无上下文 `.lock().unwrap()`（std Mutex 中毒时无消息 panic） | **fixed**：全部改为 `expect("Mutex 中毒（临界区内不应 panic）")` 补充上下文；中毒语义不变（仍 panic），临界区内无 panic 路径故实际不触发 |
| S9 | `src/rpc.rs` / `src/config.rs` / `src/git.rs` | 错误经 `RpcError`/`GitError`/`String` 显式传播 | **pass**：无静默吞错；所有外部失败路径返回错误或显式日志 |

### gui crate

| # | 位置 | 发现 | resolution |
|---|------|------|-----------|
| G5 | `src/{workflow,config}.rs` 生产代码 | 无上下文 `.lock().unwrap()` | **fixed**：同 S8，改为 `expect(...)` 带上下文 |
| G6 | `src/{ws,main}.rs` | `expect("初始化 tokio runtime 失败")` / `expect("打开窗口失败")` | **pass**：已带上下文（启动期致命错误），符合最佳实践 |
| G7 | `src/workflow.rs` | 编排引擎在 GPUI 主线程调用 tokio 依赖的 LLM（rig/reqwest） | **pass**：上一轮已修复（`run_engine_on_tokio` 桥接至 GUI tokio runtime），本轮复查无回归 |

---

## 三、尽量使用强类型

### protocol crate

| # | 位置 | 发现 | resolution |
|---|------|------|-----------|
| P5 | `src/types.rs` | 会话/活动/内容块/对话条目 | **pass**：均为 typed enum/struct（`SessionState`/`ContentBlock`/`Activity`/`DialogItem`），无 `serde_json::Value` 字段 |

### server crate

| # | 位置 | 发现 | resolution |
|---|------|------|-----------|
| S10 | `src/rpc.rs` | `handle(...) -> Result<Value, RpcError>` | **justified**：JSON-RPC 线边界返回值；入参全部 `parse::<T>` typed，出参载荷类型（`SessionMeta`/`GitDiffResult`/`ActivitiesResult`…）均在 protocol 定义并 `to_value` 序列化——`Value` 仅出现在信封载荷位置 |
| S11 | `src/transport.rs` | 信封帧解析/通知日志用 Value | **justified**：JSON-RPC 信封（id/method/params）本就是动态 JSON；params 由 rpc.rs typed 化消费，通知 payload 为 typed 序列化产物 |
| S12 | `src/agent.rs` | `skill/list` 响应用 `serde_json::Value` | **fixed**：自定义 typed 响应 `SkillListResponse { skills: Vec<SkillInfo { name }> }`（SDK 未收录方法时的官方扩展点），消费侧 `list_skills()` 语义不变 |

### gui crate

| # | 位置 | 发现 | resolution |
|---|------|------|-----------|
| G8 | `src/ws.rs` | `request(...) -> Result<Value, RpcError>` | **justified**：WS JSON-RPC 线边界（同 S10）；调用方立即 `serde_json::from_value` 到 typed 类型 |
| G9 | `src/config.rs` | `normalize(&Value)` 归一化任意用户配置 | **justified**：任意输入 JSON → 合法配置的容错归一化，`Value` 是此类「接受任意/旧/坏输入」场景的恰当类型；合法配置本身 typed（`GuiConfig`） |
| G10 | `src/workflow.rs` | rig 工具 `parameters() -> Value` | **justified**：rig `Tool` trait 的 JSON Schema 接口（第三方 API 边界），非内部松散点 |
| G11 | `src/logic.rs` | 输入附件（图片/语音 base64）用 String 字段 | **justified**：base64 数据本就是字符串载体，且 protocol `ContentBlock::Image/Resource` 已 typed 承载 |

---

## 修复改动摘要（行为保持，协议语义不变）

- `Cargo.toml`：workspace 新增 `uuid = { version = "1", features = ["v4"] }`；`rust-version` 1.80 → 1.88
- `crates/server/Cargo.toml`、`crates/gui/Cargo.toml`：新增 `uuid.workspace = true`
- `crates/server/src/session.rs`：会话 id 改 `Uuid::new_v4()`，删除手写 `uuid_v4()`
- `crates/server/src/git.rs`：revert 临时目录与测试 helper 改 uuid v4
- `crates/gui/src/workflow.rs`：编排会话 id 改 `Uuid::new_v4()`，删除手写 `uuid_v4()`
- `crates/server/src/{agent,session,transport}.rs`、`crates/gui/src/{workflow,config}.rs`：生产 std Mutex `.lock().unwrap()` → `expect(...)` 带上下文
- `crates/server/src/agent.rs`：`skill/list` 响应 Value → typed `SkillListResponse`

## 质量门禁

- `cargo check --workspace --all-targets`：0 error，0 新增 warning（仅依赖 crate 的 future-incompat 基线 1 条：block/proc-macro-error2）
- `cargo test --workspace`：全部通过（改动前基线 76 → 改动后 76）
- 工作区干净；改动已提交并推送 `origin/main`
