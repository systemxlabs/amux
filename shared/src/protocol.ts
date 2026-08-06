/**
 * amux app↔server 协议面：方法名、参数/结果类型、通知类型。
 * 语义依据：docs/DESIGN.md（§3 传输、§4 会话、§5 数据存储）与 docs/PRD.md。
 * 本文件是协议的唯一来源：server 与 app 均从这里导入。
 */

import type { Event, Input, SessionState } from "ahal";

// ---- 机器与 harness ----

export type HarnessName = "codex" | "claude" | "kimi";

export interface HarnessInfo {
  name: HarnessName;
  available: boolean;
  /** server 配置的默认模型（缺省用 harness 自身默认） */
  defaultModel?: string;
}

export interface MachineInfo {
  serverVersion: string;
  harnesses: HarnessInfo[];
}

// ---- 会话 ----

/**
 * 会话元数据（wire 形态）。
 * state 为最近已知的 AHAL 状态；interrupted 为 server 崩溃恢复标记（非 AHAL 状态）；
 * closed 表示已 close（历史保留、可 resume），delete 后从列表移除。
 */
export interface SessionMeta {
  id: string;
  harness: HarnessName;
  cwd: string;
  model?: string;
  state: SessionState;
  interrupted: boolean;
  closed: boolean;
  createdAt: number;
  /** 最近一次事件时间（epoch ms） */
  lastEventAt: number;
}

/**
 * 带 server 序号的持久化/缓冲事件。seq 按会话单调递增（重启后延续），
 * 客户端用它做重连补齐去重：跳过 seq <= 已见最大 seq 的事件。
 * event 为 ahal 事件本身，timestamp 为 server 端时间戳（epoch ms）。
 */
export interface StoredEvent {
  seq: number;
  event: Event;
  timestamp: number;
}

// ---- 方法面 ----

export const Methods = {
  GetInfo: "get_info",
  ListSessions: "list_sessions",
  CreateSession: "create_session",
  ResumeSession: "resume_session",
  CloseSession: "close_session",
  DeleteSession: "delete_session",
  Prompt: "prompt",
  Cancel: "cancel",
  GetHistory: "get_history",
  GetBufferedEvents: "get_buffered_events",
  GitStatus: "git_status",
  GitDiff: "git_diff",
  GitPush: "git_push",
  GitRevert: "git_revert",
} as const;

export type MethodName = (typeof Methods)[keyof typeof Methods];

// ---- 各方法参数/结果 ----

export interface GetInfoResult {
  info: MachineInfo;
}

export interface ListSessionsResult {
  sessions: SessionMeta[];
}

export interface CreateSessionParams {
  harness: HarnessName;
  cwd: string;
  model?: string;
}

export interface CreateSessionResult {
  session: SessionMeta;
}

export interface ResumeSessionParams {
  sessionId: string;
}

export interface ResumeSessionResult {
  session: SessionMeta;
}

export interface CloseSessionParams {
  sessionId: string;
}

export interface DeleteSessionParams {
  sessionId: string;
}

export interface PromptParams {
  sessionId: string;
  input: Input;
}

export interface CancelParams {
  sessionId: string;
}

export interface GetHistoryParams {
  sessionId: string;
}

export interface GetHistoryResult {
  events: StoredEvent[];
}

export interface GetBufferedEventsParams {
  sessionId: string;
  /** 只返回 seq 大于该值的事件；缺省返回缓冲全部（客户端总是传 lastHistorySeq 以避免重复） */
  afterSeq?: number;
}

export interface GetBufferedEventsResult {
  events: StoredEvent[];
}

// ---- git 直连（server 执行；push/revert 为无需判断的写操作）----

export interface GitStatusParams {
  cwd: string;
}

export interface GitChange {
  path: string;
  status: "added" | "modified" | "deleted" | "renamed" | "untracked";
  staged: boolean;
  additions: number;
  deletions: number;
}

export interface GitStatusResult {
  branch: string;
  changes: GitChange[];
}

export interface GitDiffParams {
  cwd: string;
  /** 缺省返回整个工作区 diff */
  path?: string;
}

export interface GitDiffResult {
  diff: string;
}

export interface GitPushParams {
  cwd: string;
}

export interface GitPushResult {
  ok: boolean;
  message?: string;
}

/**
 * GitRevertParams：撤销工作区变更（undo 语义，需会话空闲时触发）。
 * - path 与 patch 都缺省：撤销全部已跟踪变更
 * - 仅 path：撤销该文件变更（index + worktree）
 * - path + patch：对 path 反向应用 patch（hunk 级撤销；patch 取自已展示的 diff 文本）
 */
export interface GitRevertParams {
  cwd: string;
  path?: string;
  patch?: string;
}

export interface GitRevertResult {
  ok: boolean;
  message?: string;
}

// ---- 通知（server → client，无响应）----

export const Notifications = {
  Event: "event",
  SessionCreated: "session_created",
  SessionClosed: "session_closed",
  SessionInterrupted: "session_interrupted",
  SessionDeleted: "session_deleted",
} as const;

export type NotificationName = (typeof Notifications)[keyof typeof Notifications];

export interface EventNotification {
  sessionId: string;
  seq: number;
  event: Event;
  timestamp: number;
}

export interface SessionNotification {
  session: SessionMeta;
}
