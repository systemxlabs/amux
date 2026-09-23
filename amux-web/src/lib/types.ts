// 与 amux-common::api / amux-common::domain 的 JSON 表示手工对齐（camelCase，除少数按 serde 原样输出）。

export type SessionState = "idle" | "busy";

export type ContentBlock =
  | { type: "text"; text: string }
  | {
      type: "resource";
      mimeType: string;
      uri?: string;
      text?: string;
      blob?: string;
    }
  | {
      type: "resource_link";
      uri: string;
      name: string;
      mimeType?: string;
      title?: string;
      description?: string;
    };

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
      tool_call_id: string;
      tool_name: string;
      title?: string;
      parameters?: string;
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

export type Agent = { name: string; available: boolean };

export type Session = {
  id: string;
  machine: string;
  agent: string;
  title: string;
  state: SessionState;
  project?: string;
  workspace: string;
  worktreeDir: string;
  createdAt: number;
  updatedAt: number;
};

export type SessionList = { sessions: Session[]; hasMore: boolean };
export type CreateSessionRequest = {
  machine: string;
  agent: string;
  workspace: string;
  useWorktree: boolean;
  project?: string;
};
export type PromptRequest = { input: ContentBlock[] };
export type ConfigureSessionRequest = {
  title?: string;
  config?: SessionConfigSetting;
  project?: string | null;
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
export type ContextInfo = { contextSize: number; contextWindowSize: number };

export type TerminalState = "running" | "exited";
export type Terminal = {
  id: string;
  cwd: string;
  cols: number;
  rows: number;
  state: TerminalState;
};
export type TerminalOutput = { data: string; nextCursor: number; truncated?: boolean };

export type Attachment = {
  name: string;
  uri: string;
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
  createdAt: number;
  updatedAt: number;
  linkedSessions: Session[];
};
export type WorkflowList = { workflows: Workflow[]; hasMore: boolean };
export type CreateWorkflowRequest = { plan: string; title?: string; project?: string };
export type ConfigureWorkflowRequest = { title?: string; project?: string | null };

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
};

/** 会话目标：普通会话或工作流会话（对话/活动/取消等操作共用）。 */
export type OpenTarget =
  | { kind: "session"; id: string }
  | { kind: "workflow"; id: string };

/** 会话列表条目：普通会话或工作流会话（统一按最近活跃排序）。 */
export type ListEntry =
  | { kind: "session"; session: Session }
  | { kind: "workflow"; workflow: Workflow };

/** 会话实际根目录：启用 worktree 时为 worktree 目录。 */
export function rootDir(session: Session): string {
  return session.worktreeDir ? session.worktreeDir : session.workspace;
}

export function entryId(entry: ListEntry): string {
  return entry.kind === "session" ? entry.session.id : entry.workflow.id;
}

export function entryUpdatedAt(entry: ListEntry): number {
  return entry.kind === "session" ? entry.session.updatedAt : entry.workflow.updatedAt;
}

export function entryState(entry: ListEntry): SessionState {
  return entry.kind === "session" ? entry.session.state : entry.workflow.state;
}

export function entryTitle(entry: ListEntry): string {
  return entry.kind === "session" ? entry.session.title : entry.workflow.title;
}

export function entryCreatedAt(entry: ListEntry): number {
  return entry.kind === "session" ? entry.session.createdAt : entry.workflow.createdAt;
}

export function entryProject(entry: ListEntry): string | undefined {
  return entry.kind === "session" ? entry.session.project : entry.workflow.project;
}

/** 会话列表条目关联的普通会话（仅工作流会话有）。 */
export function linkedSessions(entry: ListEntry): Session[] {
  return entry.kind === "workflow" ? entry.workflow.linkedSessions : [];
}
