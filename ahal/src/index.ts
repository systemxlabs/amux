/**
 * AHAL — Agent Harness Access Layer
 *
 * 纯类型与接口包（零依赖、无运行时逻辑）。
 * 语义依据：docs/AHAL.md（框架文档）。接口签名以本包代码为准。
 */

// ---- 会话状态 ----

/** Session 的四种状态；thinking / responding / acting 统称"忙"。 */
export type SessionState = "idle" | "thinking" | "responding" | "acting";

// ---- 会话标识与选项 ----

export type SessionId = string;

export interface SessionOptions {
  /** 工作目录，必选 */
  cwd: string;
  /** 模型标识，缺省用 harness 默认 */
  model?: string;
}

// ---- 输入内容块 ----

export type TextBlock = {
  type: "text";
  text: string;
};

export type ResourceBlock =
  | { type: "resource"; mimeType: string; text: string; uri?: string }
  | { type: "resource"; mimeType: string; blob: string; uri?: string };

export type ResourceLinkBlock = {
  type: "resource_link";
  uri: string;
  name: string;
  mimeType?: string;
  title?: string;
  description?: string;
  size?: number;
};

export type ContentBlock = TextBlock | ResourceBlock | ResourceLinkBlock;

/** prompt 的输入 */
export type Input = ContentBlock[];

// ---- 事件 ----

/** 工作区间结束原因；仅 state 变为 idle 时携带 */
export type StopReason =
  | "end_turn"
  | "cancelled"
  | "max_tokens"
  | "max_turn_requests"
  | "refusal"
  | "error";

export type ToolCallStatus = "pending" | "in_progress" | "completed" | "failed" | "cancelled";

export type AgentMessage = { kind: "agent_message"; messageId: string; content?: ContentBlock[] };
export type AgentMessageChunk = { kind: "agent_message_chunk"; messageId: string; content: ContentBlock };
export type AgentThought = { kind: "agent_thought"; messageId: string; content?: ContentBlock[] };
export type AgentThoughtChunk = { kind: "agent_thought_chunk"; messageId: string; content: ContentBlock };
export type ToolCallUpdate = {
  kind: "tool_call_update";
  toolCallId: string;
  toolName?: string;
  title?: string;
  status?: ToolCallStatus;
  content?: ContentBlock[];
};
export type ToolCallContentChunk = { kind: "tool_call_content_chunk"; toolCallId: string; content: ContentBlock };
export type StateChanged = { kind: "state_changed"; state: SessionState; reason?: StopReason };
export type UsageUpdate = { kind: "usage_update"; context: number; contextWindow: number };
export type ErrorEvent = { kind: "error"; message: string };

export type Event =
  | AgentMessage
  | AgentMessageChunk
  | AgentThought
  | AgentThoughtChunk
  | ToolCallUpdate
  | ToolCallContentChunk
  | StateChanged
  | UsageUpdate
  | ErrorEvent;

/** 事件流中的一条事件，携带产生时间（epoch ms） */
export interface SessionEvent {
  event: Event;
  timestamp: number;
}

// ---- 错误类型 ----

/** AHAL 错误基类 */
export class AhalError extends Error {
  constructor(message?: string) {
    super(message);
    this.name = "AhalError";
  }
}

/** resumeSession 的 session 不存在或无法恢复 */
export class SessionNotFoundError extends AhalError {
  constructor(message?: string) {
    super(message);
    this.name = "SessionNotFoundError";
  }
}

/** 底层 harness 不可用（未安装、版本不兼容） */
export class HarnessUnavailableError extends AhalError {
  constructor(message?: string) {
    super(message);
    this.name = "HarnessUnavailableError";
  }
}

/** Driver 内部重建底层进程期间（如崩溃恢复），暂时不可写 */
export class SessionBusyError extends AhalError {
  constructor(message?: string) {
    super(message);
    this.name = "SessionBusyError";
  }
}

/** close() 后调用 Session 的任何方法 */
export class SessionClosedError extends AhalError {
  constructor(message?: string) {
    super(message);
    this.name = "SessionClosedError";
  }
}

/** steer 注入等待超时 */
export class PromptTimeoutError extends AhalError {
  constructor(message?: string) {
    super(message);
    this.name = "PromptTimeoutError";
  }
}

/** 输入非法或过大 */
export class InvalidInputError extends AhalError {
  constructor(message?: string) {
    super(message);
    this.name = "InvalidInputError";
  }
}

// ---- Driver 与 Session 接口 ----

/**
 * 适配一种 agent harness 的组件，实现本规范的全部语义。
 * 精确区分三种忙状态（thinking / responding / acting），无法满足的 harness 不接入。
 */
export interface Driver {
  /** 创建 Session，以 yolo 模式启动 agent（自动批准/屏蔽一切审批请求） */
  createSession(options: SessionOptions): Promise<Session>;
  /** 恢复已持久化的 Session（进程重启后）；无法恢复时报错；依赖底层 harness 的持久化 */
  resumeSession(sessionId: SessionId): Promise<Session>;
}

/**
 * 与某个 harness 的一段持续会话。
 *
 * prompt：唯一消息入口——idle 启动新工作，忙时 steer 注入。
 *   resolve 即已被接受并保证送达；原子性（必然送达不丢失）；有序性（按调用顺序）。
 * cancel：取消进行中的工作，阻塞到完成；已空闲则无操作。
 * close：释放资源；历史保留可恢复；之后一切调用报错。
 * events：hot stream，支持多处订阅，订阅时刻起接收后续事件；close() 后迭代器结束。
 */
export interface Session {
  readonly id: SessionId;
  readonly cwd: string;
  prompt(input: Input): Promise<void>;
  cancel(): Promise<void>;
  close(): Promise<void>;
  readonly events: AsyncIterable<SessionEvent>;
}
