// 应用状态：单一状态树，React 组件只读取与派发动作。

import type { ApiClient } from "../lib/api";
import type { Paging } from "../lib/paging";
import { newPaging } from "../lib/paging";
import { clearToken } from "../lib/token";
import type {
  Activity,
  Agent,
  Attachment,
  Automation,
  AutomationConfig,
  ContentBlock,
  Group,
  GroupMemberTemplate,
  GroupMessage,
  HistoryItem,
  ListEntry,
  Machine,
  OpenTarget,
  OrchestratorConfig,
  Project,
  QuickCommand,
  RecentWorkspace,
  Session,
  SessionConfigOption,
  SessionPlanEntry,
  Skill,
  SlashCommand,
  Terminal,
  Usage,
  Workflow,
  WorkflowPlanItem,
  WorktreeBase,
} from "../lib/types";

/** 连接状态（决定进入登录页面还是主页面）。 */
export type ConnectionStatus = "connecting" | "offline" | "failed" | "online";

/** 中间面板：新建会话视图或会话交互视图。 */
export type MiddleView = "new" | "interaction" | "automation";

/** 自动化右侧面板：新建、编辑或详情。 */
export type AutomationPanel =
  | { kind: "new" }
  | { kind: "edit"; id: string }
  | { kind: "detail"; id: string };

/** 右侧面板。 */
export type SidePanel =
  | "exec"
  | "diff"
  | "details"
  | "activities"
  | "attachments"
  | "plan"
  | "terminal";

/** 仅普通会话有的右侧面板（docs/PRD.md「主页面」）。 */
const SESSION_ONLY_PANELS: readonly SidePanel[] = ["exec", "diff", "plan", "terminal"];
/** 群聊会话没有会话活动视图。 */
const NON_GROUP_PANELS: readonly SidePanel[] = ["activities"];

/** 右侧面板是否适用于该会话。 */
export function panelAvailable(panel: SidePanel, target: OpenTarget): boolean {
  if (target.kind === "session") return true;
  if (target.kind === "group") return !NON_GROUP_PANELS.includes(panel) && !SESSION_ONLY_PANELS.includes(panel);
  return !SESSION_ONLY_PANELS.includes(panel);
}

/** 标记已读后同步清除本地未读标记（docs/DESIGN.md「会话交互视图」）。 */
export function markReadLocally(state: CoreState, kind: OpenTarget["kind"], id: string): void {
  const clearSession = (session: Session) => {
    if (session.id === id) session.unread = false;
  };
  const clearWorkflow = (workflow: Workflow) => {
    if (workflow.id !== id) return;
    workflow.unread = false;
    for (const session of workflow.linkedSessions) session.unread = false;
  };
  const clearGroup = (group: Group) => {
    if (group.id !== id) return;
    group.unread = false;
    for (const session of group.sessions) session.unread = false;
  };

  for (const entry of state.entries) {
    if (entry.kind === "session") clearSession(entry.session);
    else if (entry.kind === "workflow") clearWorkflow(entry.workflow);
    else clearGroup(entry.group);
    if (kind === "session") {
      if (entry.kind === "workflow") entry.workflow.linkedSessions.forEach(clearSession);
      if (entry.kind === "group") entry.group.sessions.forEach(clearSession);
    }
  }

  if (kind === "session") {
    if (state.detail.session) clearSession(state.detail.session);
    state.detail.workflow?.linkedSessions.forEach(clearSession);
    state.detail.group?.sessions.forEach(clearSession);
  } else if (kind === "workflow") {
    if (state.detail.workflow) clearWorkflow(state.detail.workflow);
  } else if (state.detail.group) {
    clearGroup(state.detail.group);
  }
}

export type Notice = { kind: "success" | "error"; text: string };

/** 待发送附件（已上传到 Server，发送时使用其公共 URI）。 */
export type PendingAttachment = {
  id: string;
  label: string;
  file: File;
  status: "uploading" | "failed" | "uploaded";
  controller?: AbortController;
  error?: string;
  block?: ContentBlock;
  remoteName?: string;
};

/** 新建会话视图状态（docs/PRD.md「新建会话视图」）。 */
export type NewSessionState = {
  mode: "normal" | "workflow" | "group";
  /** 选中的机器名；空串 = 未选 */
  machine: string;
  /** 选中的 agent 名；空串 = 未选 */
  agent: string;
  /** 选中的项目；undefined/空串 = 未归属 */
  project?: string;
  workspace: string;
  useWorktree: boolean;
  /** worktree 基点；null = 未选择（仓库无法识别默认分支）。 */
  worktreeBase: WorktreeBase | null;
  /** 工作流模式：计划内容（手输或选择已保存计划） */
  plan: string;
  /** 工作流模式：选中的已保存计划名 */
  selectedPlan: string | null;
  /** 群聊模式：成员草稿 */
  groupMembers: GroupMemberTemplate[];
};

/** 设置浮窗分类。 */
export type SettingsTab =
  | "connection"
  | "machines"
  | "workflow"
  | "group"
  | "automation"
  | "quickCommands"
  | "skills"
  | "projects";

/**
 * 内置智能体配置的读取状态：只有 `ready` 才表示配置已确认
 * （`config` 为 null 即未配置），未确认前不允许创建工作流会话。
 */
export type OrchestratorState =
  | { status: "loading" }
  | { status: "ready"; config: OrchestratorConfig | null }
  | { status: "failed"; error: string };

export type SettingsState = {
  open: boolean;
  tab: SettingsTab;
  machines: Machine[];
  /** 每台机器的 agent 列表（与 machines 同序） */
  agents: { machine: string; agents: Agent[] }[];
  orchestrator: OrchestratorState;
  automation: AutomationConfig | null;
  automationLoaded: boolean;
  quickCommands: QuickCommand[];
  skills: Skill[];
  plans: WorkflowPlanItem[];
  groupMembers: GroupMemberTemplate[];
  projects: Project[];
};

/** 终端输出增量（供 xterm.js 写入）。 */
export type TerminalChunk = { seq: number; bytes: Uint8Array; reset: boolean };

/** 打开会话的明细数据（普通会话与工作流会话共用）。 */
export type DetailState = {
  session: Session | null;
  workflow: Workflow | null;
  group: Group | null;
  history: HistoryItem[];
  historyPaging: Paging;
  groupHistory: GroupMessage[];
  groupHistoryPaging: Paging;
  activities: Activity[];
  activitiesPaging: Paging;
  plan: SessionPlanEntry[];
  configOptions: SessionConfigOption[];
  slashCommands: SlashCommand[];
  ongoing: Activity | null;
  attachments: Attachment[];
  attachmentsHasMore: boolean;
  attachmentsLoading: boolean;
  usage: Usage | null;
  terminals: Terminal[];
  activeTerminal: string | null;
  terminalChunks: TerminalChunk[];
  terminalSeq: number;
};

export type CoreState = {
  status: ConnectionStatus;
  /** 连接错误（本地无 token 且未尝试登录时为 null，登录页据此不展示提示） */
  error: string | null;
  notice: Notice | null;
  entries: ListEntry[];
  /** 会话列表成功刷新的版本号；项目列表据此执行静默刷新 */
  listRefreshVersion: number;
  listPaging: Paging;
  /** 展开的工作流/群聊会话 id */
  expanded: string[];
  middle: MiddleView;
  open: OpenTarget | null;
  sidePanel: SidePanel | null;
  detail: DetailState;
  /** 自动化列表 */
  automations: Automation[];
  /** 自动化“已有会话”下拉框的全量候选项。 */
  automationSessions: Session[];
  automationWorkflows: Workflow[];
  automationGroups: Group[];
  automationSessionsHasMore: boolean;
  automationWorkflowsHasMore: boolean;
  automationGroupsHasMore: boolean;
  automationSessionsLoading: boolean;
  automationWorkflowsLoading: boolean;
  automationGroupsLoading: boolean;
  automationSessionOffset: number;
  automationWorkflowOffset: number;
  automationGroupOffset: number;
  /** 自动化右侧面板；null = 收起 */
  automationPanel: AutomationPanel | null;
  /** 会话交互视图输入框草稿（改动审查引用会往这里追加） */
  inputDraft: string;
  newSession: NewSessionState;
  settings: SettingsState;
  recentWorkspaces: RecentWorkspace[];
  attachments: PendingAttachment[];
};

export function initialDetail(): DetailState {
  return {
    session: null,
    workflow: null,
    group: null,
    history: [],
    historyPaging: newPaging(),
    groupHistory: [],
    groupHistoryPaging: newPaging(),
    activities: [],
    activitiesPaging: newPaging(),
    plan: [],
    configOptions: [],
    slashCommands: [],
    ongoing: null,
    attachments: [],
    attachmentsHasMore: false,
    attachmentsLoading: false,
    usage: null,
    terminals: [],
    activeTerminal: null,
    terminalChunks: [],
    terminalSeq: 0,
  };
}

export function initialNewSession(): NewSessionState {
  return {
    mode: "normal",
    machine: "",
    agent: "",
    project: undefined,
    workspace: "",
    useWorktree: false,
    worktreeBase: null,
    plan: "",
    selectedPlan: null,
    groupMembers: [],
  };
}

export function initialSettings(): SettingsState {
  return {
    open: false,
    tab: "connection",
    machines: [],
    agents: [],
    orchestrator: { status: "loading" },
    automation: null,
    automationLoaded: false,
    quickCommands: [],
    skills: [],
    plans: [],
    groupMembers: [],
    projects: [],
  };
}

export function initialState(): CoreState {
  return {
    status: "connecting",
    error: null,
    notice: null,
    entries: [],
    listRefreshVersion: 0,
    listPaging: newPaging(),
    expanded: [],
    middle: "new",
    open: null,
    sidePanel: null,
    detail: initialDetail(),
    automations: [],
    automationSessions: [],
    automationWorkflows: [],
    automationGroups: [],
    automationSessionsHasMore: false,
    automationWorkflowsHasMore: false,
    automationGroupsHasMore: false,
    automationSessionsLoading: false,
    automationWorkflowsLoading: false,
    automationGroupsLoading: false,
    automationSessionOffset: 0,
    automationWorkflowOffset: 0,
    automationGroupOffset: 0,
    automationPanel: null,
    inputDraft: "",
    newSession: initialNewSession(),
    settings: initialSettings(),
    recentWorkspaces: [],
    attachments: [],
  };
}

/** 各视图的上次刷新时刻（毫秒时间戳；不参与渲染）。 */
export type Ticks = {
  list?: number;
  automations?: number;
  history?: number;
  ongoing?: number;
  activities?: number;
  plan?: number;
  usage?: number;
  reconnect?: number;
};

/** 应用状态容器：可变状态 + 版本号，React 通过 `useSyncExternalStore` 订阅。 */
export class Core {
  state: CoreState = initialState();
  client: ApiClient | null = null;
  last: Ticks = {};
  version = 0;
  private listeners = new Set<() => void>();
  private noticeTimer: ReturnType<typeof setTimeout> | null = null;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getVersion = (): number => this.version;

  /** 修改状态并通知订阅者。 */
  update(mutator: (state: CoreState) => void): void {
    mutator(this.state);
    this.emit();
  }

  emit(): void {
    this.version += 1;
    for (const listener of this.listeners) listener();
  }

  /** 追加一条按序的终端输出；React 在单个渲染批次中也不会丢失中间事件。 */
  pushTerminalChunk(bytes: Uint8Array, reset: boolean): void {
    this.update((state) => {
      state.detail.terminalSeq += 1;
      state.detail.terminalChunks = [
        ...state.detail.terminalChunks,
        { seq: state.detail.terminalSeq, bytes, reset },
      ];
    });
  }

  /** 已写入 xterm.js 的事件可移除；确认值后的新事件保留。 */
  acknowledgeTerminalChunks(seq: number): void {
    if (
      seq === 0 ||
      this.state.detail.terminalChunks.every((chunk) => chunk.seq > seq)
    ) {
      return;
    }
    this.update((state) => {
      state.detail.terminalChunks = state.detail.terminalChunks.filter(
        (chunk) => chunk.seq > seq,
      );
    });
  }

  /** 通知提示：成功/失败，数秒后自动消失。 */
  notify(kind: Notice["kind"], text: string): void {
    if (this.noticeTimer !== null) clearTimeout(this.noticeTimer);
    this.update((state) => {
      state.notice = { kind, text };
    });
    this.noticeTimer = setTimeout(() => {
      this.noticeTimer = null;
      this.update((state) => {
        state.notice = null;
      });
    }, 4000);
  }

  success(text: string): void {
    this.notify("success", text);
  }

  failure(text: string): void {
    this.notify("error", text);
  }

  /** 重置打开会话的明细与逐视图节拍（切换会话/连接重建时调用）。 */
  resetDetail(): void {
    const terminalSeq = this.state.detail.terminalSeq;
    this.state.detail = initialDetail();
    this.state.detail.terminalSeq = terminalSeq;
  }

  /** 清空所有定时刷新节拍，使下次 tick 立即拉取。 */
  resetTicks(): void {
    this.last = {};
  }

  /** 当前连接认证失效：清除凭据与定时请求，并立即进入登录页面。 */
  invalidateAuthentication(client: ApiClient): void {
    if (this.client !== client) return;
    this.client = null;
    clearToken();
    this.last = {};
    if (this.noticeTimer !== null) {
      clearTimeout(this.noticeTimer);
      this.noticeTimer = null;
    }
    this.update((state) => {
      state.status = "failed";
      state.error = "登录已失效，请重新输入 token";
      state.notice = null;
    });
  }
}
