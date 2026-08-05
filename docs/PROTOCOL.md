# amux — Client ↔ Daemon 通信协议

版本: 1.0 (草案)
日期: 2026-08-05

本协议定义 amux 桌面客户端（GUI）与机器 daemon 之间的通信契约。daemon 内部通过 [AHAL](AHAL.md) 与 agent harness 交互；本协议是 daemon 对上层暴露的唯一接口面。

---

## 1. 角色与拓扑

- **Client（仅 GUI 客户端）**：amux 桌面应用，是唯一的客户端。**直连各已注册机器的 daemon**——本机与远程同等对待，统一注册后连接（见 PRD「多机器管理」）。每条连接对应一台机器，使用同一套协议。
- **Daemon**：每台机器运行一个 daemon（系统服务），持有本机 AHAL Driver 实例，负责会话生命周期、事件持久化、只读 git 能力。**daemon 之间不通信**——每个 daemon 只服务本机会话，对连接方一律按 GUI 客户端对待。

```
┌──────────────┐   WS(注册)   ┌──────────────┐
│              │────────────►│ 机器 A daemon │──► AHAL → harness
│  GUI 桌面     │             └──────────────┘
│  (聚合视图)   │   WS(注册)   ┌──────────────┐
│              │────────────►│ 机器 B daemon │──► AHAL → harness
└──────────────┘             └──────────────┘
                     （机器 A/B 无本机/远程之分，含本机）
```

职责划分：

- **会话数据主权**：每个会话的历史事件流、元数据只存于**该会话所在机器**的 daemon；GUI 不复制远程会话历史。
- **GUI = 聚合层**：会话列表由 GUI 汇总各 daemon 的连接编排（见 §10 机器注册）；跨机器工作流是 GUI 内部编排，仅基于 §4-§5 的会话原语，不占用协议面。远程 daemon 离线时，其会话在 GUI 中标为不可达，注册表保留。
- 多设备共存：任意数量的 GUI 可同时接收同一会话的事件流，各自维护本地游标，互不踢出。
- **daemon 生命周期与客户端连接解耦**：任何客户端断开（含桌面应用关闭）不停止 daemon、不销毁会话；会话仅由显式 `session.close` / `session.delete` 结束。daemon 由所在机器自行启动，GUI 不负责拉起（见 DESIGN.md「Daemon 生命周期与启动」）。

## 2. 传输与安全

- 传输**统一为 WebSocket**（文本帧，UTF-8），消息格式为 JSON-RPC 2.0（§3）。默认端口 `19770`（可配置）：
  - 所有机器（含本机）同等对待，注册 / 连接 / 认证流程一致；本机 URL 为 `ws://127.0.0.1:19770`
  - 认证统一：token 必填（安装 daemon 时生成，保存在 `~/.amux/token`，仅展示一次）；本机走 `ws://`、远程建议 `wss://`（TLS）
- 心跳：使用 WebSocket 层 ping/pong；断线后客户端指数退避重连，重连后按游标补齐（见 §6）。
- 二进制：内容块中的 `blob` 以 base64 内嵌 JSON（建议单条 ≤ 10MB）；后续如需大文件可增加二进制帧通道（本版不定义）。

## 3. 消息格式

消息采用 **JSON-RPC 2.0**，每条消息一个 WebSocket 文本帧。三类消息：

```jsonc
// 请求（GUI → daemon）——必须回响应
{ "jsonrpc": "2.0", "id": 1, "method": "session.create", "params": { "cwd": "/srv/app" } }

// 响应（成功）
{ "jsonrpc": "2.0", "id": 1, "result": { "id": "m1:s7" } }

// 响应（失败）
{ "jsonrpc": "2.0", "id": 1,
  "error": { "code": -32008, "message": "codex 未安装",
             "data": { "code": "HARNESS_UNAVAILABLE" } } }

// 通知（无 id、无响应）——daemon → client 单向推送
{ "jsonrpc": "2.0", "method": "session.event",
  "params": { "sessionId": "m1:s7", "seq": 42, "ts": 1753987200000,
              "payload": { "kind": "state_changed", "state": "idle", "reason": "end_turn" } } }
```

- **协议版本**：JSON-RPC 版本由 `jsonrpc` 字段标识。v1 不做应用协议版本协商——daemon 较旧时，其不认识的方法以 `-32601`（method not found）自然暴露。
- **事件一律用通知表达**：`session.event` 等。通知不要求也不允许响应。
- **错误对象**：`code` 为整数（JSON-RPC 规范要求，§11 定义）；`message` 供展示；`data.code` 为稳定字符串错误码，便于程序分支，与 AHAL 错误类一一对应。
- 不使用批量请求（batch）；每条消息独立成帧。

**会话 ID**：各 daemon 分配本机唯一的本地 Id；GUI 聚合展示时以 `{machineId}:{本地Id}` 区分不同机器（machineId 来自 `daemon.getConfig`，或注册时分配）。协议方法只认本机 Id——GUI 把请求发给会话所属的 daemon 连接。

## 4. 会话生命周期

| method | params | result | 说明 |
|---|---|---|---|
| `session.create` | `{ cwd, model?, skills?: SkillsRef[], initialPrompt?: ContentBlock[] }` | `{ id }` | `skills` 为 GUI 本地注册表中已启用的条目（见 §9），daemon 过滤拼装后随初始消息发出；`initialPrompt` 可选，创建后立即作为新工作发送 |
| `session.resume` | `{ id }` | `{ id, state }` | 恢复已持久化会话；失败 `SESSION_NOT_FOUND` |
| `session.close` | `{ id }` | `{ }` | 停止 agent、释放资源，历史保留可恢复 |
| `session.kill` | `{ id }` | `{ }` | cancel 进行中工作 + 立即 close（Kill Session 按钮） |
| `session.delete` | `{ id }` | `{ }` | 从注册表移除并删除历史（不可恢复，二次确认） |
| `session.list` | `{ }` | `{ sessions: SessionSummary[] }` | 本 daemon 上的会话；GUI 合并各连接的结果 |
| `session.query` | `{ harness?, state?, keyword?, tag?, archived? }` | `{ sessions: [...] }` | 本 daemon 范围内筛选（P1） |
| `session.meta.set` | `{ id, tags?, note?, pinned?, archived? }` | `{ }` | 标签/备注/置顶/归档（P1） |

```jsonc
SessionSummary = {
  "id": "m1:s7", "machineId": "m1", "machineOnline": true,
  "harness": "codex", "cwd": "/srv/app", "model": null,
  "state": "thinking",                      // idle|thinking|responding|acting
  "lastSeq": 42, "lastActivityTs": 1753987200000,
  "summary": "修复 login 的测试失败…",       // 最近一条文本片段，列表展示用
  "pinned": false, "archived": false, "tags": [], "note": null
}
```

**重启恢复**：daemon 重启后从磁盘注册表重建会话列表；崩溃前处于忙状态的会话标为 `state: "idle"` + `interrupted: true`（对应 AHAL"上一段工作结局未知"），由客户端决定是否 `resume`。

## 5. 会话交互

| method | params | result | 错误 |
|---|---|---|---|
| `session.prompt` | `{ id, content: ContentBlock[], mode?: "immediate"\|"after_idle", timeoutMs? }` | `{ accepted: true }` | `PROMPT_TIMEOUT`, `SESSION_BUSY`, `SESSION_CLOSED`, `INVALID_INPUT` |
| `session.cancel` | `{ id }` | `{ cancelled: bool }` | `SESSION_CLOSED` |

- `content` 与 `Input` 同构（AHAL `ContentBlock[]`：text / resource / resource_link）。
- **`mode` 语义**（server 侧策略）：
  - `immediate`（默认）：转发给 AHAL `prompt()`——idle 则启动新工作，忙则 steer。超时（默认 5s）→ `PROMPT_TIMEOUT`，客户端可重发。
  - `after_idle`：server 入队，等该会话 `state_changed(state=idle)` 后再发送（AHAL follow-up 约定）。同一会话队列先进先出，与其它 prompt 按到达顺序合并。
- **按钮映射（客户端本地配置，无专用协议）**：
  - commit / push / submit PR / undo / revert（单文件 / hunk / 全部）→ `session.prompt`，模板文本（revert 携带目标文件与 hunk 内容）+ `mode: after_idle`（undo/revert 必须等 idle，否则"撤销最近变更"的时点语义是乱的）
  - 新会话 / Kill Session → `session.create` / `session.kill`
  - 在 diff 视图对代码片段发 prompt → `session.prompt`（片段作为 text 块）
- **多客户端并发**：server 对同一会话的所有 prompt（含各客户端的）按到达顺序串行化后转发 AHAL，保证"按调用顺序送达"。
- **通知（P1）**：客户端从事件流自行推导（工作结束/异常/长时间无响应=忙状态且 X 分钟无事件），配置存客户端本地，无需协议。

## 6. 事件流与历史

### 实时推送（广播）

- daemon 向**所有已连接客户端**持续推送每条 `session.event` **通知**（实时事件 = AHAL `SessionEvent` 透传 + `seq`），**连接即收流，无订阅机制**。
- 多客户端并存：所有客户端收到同一份事件流，各自维护本地游标（seq），互不踢出。
- 客户端断开期间错过的事件，重连后自行补齐（见下）。

### 历史（P0，daemon 本地持久化）

| method | params | result |
|---|---|---|
| `session.history` | `{ id, afterSeq?, beforeSeq?, limit? }` | `{ events: [...], hasMore: bool }` | 滚动回溯（beforeSeq）/ 断线补齐（afterSeq）/ 区间拉取 |
| `session.search` | `{ id, query, limit? }` | `{ matches: [{ seq, ts, kind, snippet }] }` | 历史文本搜索 |

- **重连补齐**：客户端记录各会话最后一条 seq；重连后对关心的会话调 `session.history { id, afterSeq: 最后seq }` 拉取缺口，之后由广播流续上。不关心的会话跳过补齐即可（PRD"跳到最新、可上滚回溯"由客户端控制）。
- 单次分页上限默认 10k 条事件。

### 事件顺序保证

- 每个会话的 `seq` 单调递增，由会话所在 daemon 在**持久化时**分配；广播推送顺序 = seq 顺序 = AHAL 投递顺序。
- 跨机器统一时间线用 `ts`（各机时钟偏差为已知限制，展示层容忍）。

## 7. 只读 git / diff（P1）

写操作全部走 agent prompt（§5）——commit/push/undo/revert 均通过 `session.prompt` 下发，daemon 不做任何 git 写操作；**读操作由 daemon 直连 git**（GUI 无法靠 prompt 实时渲染 diff）。revert 时客户端把目标文件与 hunk 内容拼入提示词。

| method | params | result |
|---|---|---|
| `diff.query` | `{ id, base?: "session_start"\|"head" }` | `{ files: [{ path, status, added, removed }], summary }` |
| `diff.file` | `{ id, path, base? }` | `{ path, hunks: [{ hunkId, oldStart, newStart, lines: [{type, text}] }] }` | side-by-side / inline 渲染 |
| `diff.compare` | `{ idA, idB }` | 两会话基线差异合并视图（P1） |

- `base: "session_start"` = 会话创建（或最近一次 resume）时的 git 基线，daemon 创建会话时快照；`head` = 当前 HEAD。
- `diff.query` / `diff.file` 需要 daemon 具备 git 只读能力（`daemon.getConfig` 的 `gitRead`）。

## 8. Worktree（P2）

| method | params | result |
|---|---|---|
| `worktree.list` | `{ repoPath? }` | `[{ path, branch, commit, clean, locked, sessionId? }]` |
| `worktree.create` | `{ repoPath, branch?, commit?, withSession?: { model? } }` | `{ path, sessionId? }` |
| `worktree.delete` | `{ path }` | `{ }` | 有活跃会话时警告，需 `force: true` |
| `worktree.lock` | `{ path, locked }` | `{ }` |

## 9. Skills（P2）

Skills 注册表（URL + 本地目录 + 作用域 + 启用状态）是用户配置，存于 **GUI 客户端**（同机器注册表，见 §10），不在协议面——增删/启停是 GUI 本地操作，多设备各自配置。

唯一的跨边界交互在会话创建：GUI 把已启用条目经 `session.create.skills` 传给 daemon，daemon 按作用域过滤后拼成 prompt 随初始消息发出；clone/pull 由 agent 执行。

```jsonc
SkillsRef = { "url": string, "localDir": string,
              "scope"?: "global" | "project" | "personal",
              "repoIdentity"?: string }   // project 作用域时用于匹配 cwd 所属 repo（如 remote URL）
```

- `global` / `personal` 条目始终拼入；`project` 条目仅当 daemon 从 cwd 解析出的 repo 与 `repoIdentity` 匹配时拼入。
- daemon 只读这两项并拼装，不存储（DESIGN 原则）。

## 10. 机器注册（P0）

注册表存于 **GUI 客户端**（本地配置文件，如 `~/.amux/machines.json`：machineId、名称、url、token）。GUI 对每台已注册机器建立一条独立 WebSocket 连接，在线状态由连接结果判断。daemon 侧只提供本机配置接口：

| method | params | result |
|---|---|---|
| `daemon.getConfig` | `{ }` | `{ machineId, harnesses: [{ name, path, version }], gitRead, defaultModels, port }` | daemon 标识（machineId）、自动发现的 harness 与当前配置 |
| `daemon.setConfig` | `{ harnessPaths?, defaultModels? }` | `{ }` | 手动配置 harness 路径与默认模型（PRD P0） |

接入流程：机器（含本机）安装并启动 daemon → 生成 token（仅展示一次）→ GUI 本地登记 `{ url, token, name }` → GUI 发起连接 → 连接成功后该机器上线（在线状态在 GUI 的机器列表可见）。

## 11. 错误码

`error.code` 为整数（JSON-RPC 规范要求），`error.data.code` 为稳定字符串码，便于程序分支。

### JSON-RPC 标准码

| code | 含义 |
|---|---|
| `-32700` | 解析错误（parse error） |
| `-32600` | 无效请求（invalid request） |
| `-32601` | 方法不存在（method not found） |
| `-32602` | 参数非法（invalid params） |
| `-32603` | 内部错误（internal error） |

### 应用错误码（-32000 系列）

| code | data.code | 对应 AHAL | 说明 |
|---|---|---|---|
| `-32000` | `AUTH_REQUIRED` | — | 需要认证 |
| `-32001` | `AUTH_FAILED` | — | token 无效 |
| `-32003` | `SESSION_NOT_FOUND` | `SessionNotFoundError` | resume / 操作不存在的会话 |
| `-32004` | `SESSION_BUSY` | `SessionBusyError` | daemon 重建底层进程期间暂不可写 |
| `-32005` | `SESSION_CLOSED` | `SessionClosedError` | 会话已关闭 |
| `-32006` | `PROMPT_TIMEOUT` | `PromptTimeoutError` | steer 注入超时（可能已注入，客户端可重发） |
| `-32007` | `INVALID_INPUT` | `InvalidInputError` | 输入非法或过大 |
| `-32008` | `HARNESS_UNAVAILABLE` | `HarnessUnavailableError` | 未安装 / 版本不兼容 |
| `-32009` | `MACHINE_OFFLINE` | — | 目标远程 daemon 不在线（转发失败） |
| `-32010` | `NOT_IMPLEMENTED` | — | 能力未启用（如 `gitRead: false`） |

## 12. 与 AHAL 的对应关系

| 本协议 | AHAL（daemon 内部调用） |
|---|---|
| `session.create` / `session.resume` / `session.close` | `Driver.createSession` / `resumeSession` / `Session.close` |
| `session.prompt`（`immediate`） | `Session.prompt`（idle 启动 / 忙时 steer） |
| `session.prompt`（`after_idle`） | 等 `state_changed(idle)` 后再 `prompt`（follow-up 约定） |
| `session.cancel` | `Session.cancel` |
| `session.event` 帧 `payload` | `SessionEvent`（`agent_message*`、`agent_thought*`、`tool_call_*`、`state_changed`、`usage_update`、`error`）逐条透传 |
| `seq` | daemon 持久化时分配，非 AHAL 字段 |
| 错误码 | 一一映射 §11 |
