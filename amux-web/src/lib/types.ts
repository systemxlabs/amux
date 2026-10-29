// 与 amux-common::api / amux-common::domain 的 JSON 表示手工对齐（camelCase，除少数按 serde 原样输出）。

import type { ContentBlock } from "@agentclientprotocol/sdk";

export type { ContentBlock };

export type SessionState = "idle" | "busy";

export type HistoryItem = {
  role: "user" | "agent";
  id: string;
  content: ContentBlock[];
  timestamp: number;
};

export type Activity =
  | { kind: "thinking"; id: string; timestamp: number; thinking: string }
  | {
      kind: "tool_call";
      id: string;
      timestamp: number;
      tool_call_id?: string;
      tool_name?: string;
      title?: string;
      parameters?: string;
    }
  | { kind: "compaction"; id: string; timestamp: number; status: string; summary: string }
  | {
      kind: "notice";
      id: string;
      timestamp: number;
      severity: string;
      title: string;
      description?: string;
    }
  | { kind: "error"; id: string; timestamp: number; error: string };

export type SessionConfigSelectEntry = { value: string; name: string };

export type SessionConfigKind =
  | { type: "select"; current_value: string; options: SessionConfigSelectEntry[] }
  | { type: "boolean"; current_value: boolean };

export type SessionConfigOption = {
  id: string;
  name: string;
  description?: string;
  category?: string;
} & SessionConfigKind;

export type SessionConfigOptionValue =
  | { type: "value_id"; value: string }
  | { type: "boolean"; value: boolean };

export type SessionConfigSetting = { configId: string } & SessionConfigOptionValue;

export type SlashCommand = { name: string; description: string; hint?: string };

export type SessionPlanPriority = "high" | "medium" | "low";
export type SessionPlanStatus = "pending" | "in_progress" | "completed";

export type SessionPlanEntry = {
  content: string;
  priority: SessionPlanPriority;
  status: SessionPlanStatus;
};

export type GitChangeStatus = "added" | "modified" | "deleted";
export type GitDiffLineKind = "context" | "add" | "remove";

export type GitDiffLine = { kind: GitDiffLineKind; text: string };
export type GitDiffHunk = { header: string; lines: GitDiffLine[] };
export type GitDiffFile = {
  path: string;
  status: GitChangeStatus;
  additions: number;
  deletions: number;
  hunks: GitDiffHunk[];
};
export type GitDiffResult = { files: GitDiffFile[]; notRepo?: boolean };
export type GitBranchKind = "local" | "remote";
type GitBranchBase = {
  name: string;
  fullName: string;
  isWorktreeBase: boolean;
  isRemoteDefault: boolean;
};
export type GitBranch =
  | (GitBranchBase & { kind: "local" })
  | (GitBranchBase & {
      kind: "remote";
      remote: string;
      remoteBranch: string;
    });
export type GitBranchListResult = { currentBranch: string; branches: GitBranch[] };

/** worktree 基分支的完整引用名，如 `refs/heads/main`。 */
export type WorktreeBase = string;

export type FsEntry = { name: string; path: string; isDir: boolean; size: number };
export type FsListResult = {
  path: string;
  entries: FsEntry[];
  hasMore: boolean;
  nextOffset: number;
};
export type FsReadResult = {
  path: string;
  content: string;
  hasMore: boolean;
  nextOffset: number;
};

export type Machine = {
  name: string;
  os: string;
  arch: string;
  hostname: string;
  tempDir: string;
  version: string;
};

export type Agent = {
  name: string;
  available: boolean;
  openedSessions: number;
};

/** Agent 本地配置中的模型候选。 */
export type AgentModel = {
  value: string;
  name: string;
};

export type Session = {
  id: string;
  machine: string;
  agent: string;
  model?: string;
  title: string;
  state: SessionState;
  project?: string;
  pinned: boolean;
  unread: boolean;
  workspace: string;
  worktreeDir: string;
  createdAt: number;
  updatedAt: number;
};

export type SessionList = { sessions: Session[]; hasMore: boolean };
export type CreateSessionRequest = {
  machine: string;
  agent: string;
  model?: string;
  workspace: string;
  useWorktree: boolean;
  worktreeBase?: WorktreeBase;
  project?: string;
};
export type PromptRequest = { input: ContentBlock[] };
export type ConfigureSessionRequest = {
  title: string;
  config: SessionConfigSetting | null;
  project: string | null;
  pinned: boolean;
};

export type HistoryPage = {
  items: HistoryItem[];
  hasMore: boolean;
  nextOffset?: number;
};
export type ActivitiesPage = {
  activities: Activity[];
  hasMore: boolean;
  nextOffset?: number;
};
export type OngoingActivity = { activity?: Activity };
export type ConfigOptions = { options: SessionConfigOption[] };
export type SlashCommands = { commands: SlashCommand[] };
export type Plan = { entries: SessionPlanEntry[] };
export type Usage = { used: number; size: number };

export type TerminalState = "running" | "exited";
export type Terminal = {
  id: string;
  cwd: string;
  cols: number;
  rows: number;
  state: TerminalState;
};
export type TerminalOutput = {
  data: string;
  sequence: number;
  truncated?: boolean;
  exited?: boolean;
};

export type Attachment = {
  name: string;
  size: number;
  createdAt: number;
};
export type AttachmentList = { attachments: Attachment[]; hasMore: boolean };

export type Workflow = {
  id: string;
  title: string;
  state: SessionState;
  plan: string;
  project?: string;
  pinned: boolean;
  unread: boolean;
  createdAt: number;
  updatedAt: number;
  linkedSessions: Session[];
};
export type WorkflowList = { workflows: Workflow[]; hasMore: boolean };
export type CreateWorkflowRequest = { plan: string; title?: string; project?: string };
export type ConfigureWorkflowRequest = {
  title: string;
  project: string | null;
  pinned: boolean;
};

/** 群聊预设成员。 */
export type GroupMemberTemplate = {
  name: string;
  description: string;
  machine: string;
  agent: string;
  /** 模型偏好；undefined = 使用 Agent 默认模型 */
  model?: string;
  workspace: string;
  useWorktree: boolean;
  worktreeBase?: WorktreeBase;
};

export type GroupMember = {
  id: string;
  name: string;
  description: string;
  machine: string;
  agent: string;
  sessionId: string;
  pinned: boolean;
  createdAt: number;
  updatedAt: number;
};

export type Group = {
  id: string;
  title: string;
  state: SessionState;
  project?: string;
  pinned: boolean;
  unread: boolean;
  createdAt: number;
  updatedAt: number;
  members: GroupMember[];
  sessions: Session[];
};

export type GroupList = { groups: Group[]; hasMore: boolean };
export type CreateGroupRequest = {
  members: GroupMemberTemplate[];
  project?: string;
};
export type ConfigureGroupRequest = {
  title: string;
  project: string | null;
  pinned: boolean;
};

/** 群聊消息：服务端已解析成员会话引用。 */
export type GroupMessage = {
  id: string;
  memberId?: string;
  name: string;
  agent?: string;
  description?: string;
  content: ContentBlock[];
  timestamp: number;
};

export type GroupHistoryPage = {
  items: GroupMessage[];
  hasMore: boolean;
  nextOffset?: number;
};

/** 管理类操作的通用应答。 */
export type OpAck = { ok: boolean };

export type Skill = { name: string; description: string };
export type WorkflowPlanItem = {
  name: string;
  plan: string;
  lastUsedProject?: string;
};
export type Project = { name: string; description: string };
export type RecentWorkspace = {
  machine: string;
  workspace: string;
  lastUsedProject?: string;
  lastUsed: number;
};
export type QuickCommand = { project?: string; name: string; prompt: string };
export type ApiFormat = "chat_completions" | "responses" | "messages";
export type OrchestratorConfig = {
  apiFormat: ApiFormat;
  baseUrl: string;
  apiKey: string;
  model: string;
  effort: string;
  contextWindow: number;
};

export type AutomationTrigger =
  | { type: "schedule"; cron: string }
  | { type: "github_webhook"; repository: string; events?: string[] };

export type AutomationSessionOptions =
  | { mode: "existing_session"; sessionId: string }
  | { mode: "existing_workflow"; workflowId: string }
  | { mode: "existing_group"; groupId: string }
  | {
      mode: "new_session";
      machine: string;
      agent: string;
      model?: string;
      workspace: string;
      useWorktree: boolean;
      worktreeBase?: WorktreeBase;
      project?: string;
    }
  | { mode: "new_workflow"; plan: string; project?: string }
  | { mode: "new_group"; members: GroupMemberTemplate[]; project?: string };

export type AutomationRunState = "queued" | "running" | "finished" | "failed";

export type AutomationRun = {
  id: string;
  automationId: string;
  sessionId?: string;
  workflowId?: string;
  groupId?: string;
  state: AutomationRunState;
  resultLabel?: string;
  error?: string;
  createdAt: number;
  startedAt?: number;
  finishedAt?: number;
};

export type AutomationResultLabel = { name: string; description: string };

export type Automation = {
  id: string;
  title: string;
  enabled: boolean;
  task: string;
  trigger: AutomationTrigger;
  sessionOptions: AutomationSessionOptions;
  resultLabels: AutomationResultLabel[];
  createdAt: number;
  updatedAt: number;
  nextRunAt?: number;
  lastRun?: AutomationRun;
};

export type AutomationInput = {
  title: string;
  enabled: boolean;
  task: string;
  trigger: AutomationTrigger;
  sessionOptions: AutomationSessionOptions;
  resultLabels: AutomationResultLabel[];
};

export type AutomationRunList = { runs: AutomationRun[]; hasMore: boolean };

export type AutomationApiFormat = "system_one";
export type AutomationConfig = {
  apiFormat: AutomationApiFormat;
  endpoint: string;
  apiKey: string;
  model: string;
};

/** 会话目标：普通、工作流或群聊会话（对话/活动/取消等操作共用）。 */
export type OpenTarget =
  | { kind: "session"; id: string }
  | { kind: "workflow"; id: string }
  | { kind: "group"; id: string };

/** 会话列表条目：普通、工作流或群聊会话（置顶优先，其余按创建时间排序）。 */
export type ListEntry =
  | { kind: "session"; session: Session }
  | { kind: "workflow"; workflow: Workflow }
  | { kind: "group"; group: Group };

/** 会话实际执行目录：启用 worktree 时为 worktree 目录。 */
export function execDir(session: Session): string {
  return session.worktreeDir ? session.worktreeDir : session.workspace;
}

export function entryId(entry: ListEntry): string {
  if (entry.kind === "session") return entry.session.id;
  if (entry.kind === "workflow") return entry.workflow.id;
  return entry.group.id;
}

export function entryState(entry: ListEntry): SessionState {
  if (entry.kind === "session") return entry.session.state;
  if (entry.kind === "workflow") return entry.workflow.state;
  return entry.group.state;
}

export function entryTitle(entry: ListEntry): string {
  if (entry.kind === "session") return entry.session.title;
  if (entry.kind === "workflow") return entry.workflow.title;
  return entry.group.title;
}

export function entryPinned(entry: ListEntry): boolean {
  if (entry.kind === "session") return entry.session.pinned;
  if (entry.kind === "workflow") return entry.workflow.pinned;
  return entry.group.pinned;
}

export function entryUnread(entry: ListEntry): boolean {
  if (entry.kind === "session") return entry.session.unread;
  if (entry.kind === "workflow") return entry.workflow.unread;
  return entry.group.unread;
}

export function entryCreatedAt(entry: ListEntry): number {
  if (entry.kind === "session") return entry.session.createdAt;
  if (entry.kind === "workflow") return entry.workflow.createdAt;
  return entry.group.createdAt;
}

export function entryProject(entry: ListEntry): string | undefined {
  if (entry.kind === "session") return entry.session.project;
  if (entry.kind === "workflow") return entry.workflow.project;
  return entry.group.project;
}

/** 会话列表条目关联的普通会话（工作流/群聊会话有）。 */
export function linkedSessions(entry: ListEntry): Session[] {
  if (entry.kind === "workflow") return entry.workflow.linkedSessions;
  if (entry.kind === "group") return entry.group.sessions;
  return [];
}
