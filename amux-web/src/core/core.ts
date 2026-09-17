// 应用状态：与桌面应用同构的单一状态树，React 组件只读取与派发动作。

import type { ApiClient } from "../lib/api";
import type { Paging } from "../lib/paging";
import { newPaging } from "../lib/paging";
import { newTerminalStream, type TerminalStream } from "../lib/terminal";
import type {
  Activity,
  Agent,
  ContentBlock,
  FsEntry,
  HistoryItem,
  ListEntry,
  Machine,
  OpenTarget,
  OrchestratorConfig,
  QuickCommand,
  Session,
  SessionConfigOption,
  SessionPlanEntry,
  Skill,
  SlashCommand,
  Terminal,
  Workflow,
  WorkflowPlanItem,
} from "../lib/types";

/** 连接状态（决定进入登录页面还是主页面）。 */
export type ConnectionStatus = "connecting" | "offline" | "failed" | "online";

/** 中间面板：新建会话视图或会话交互视图。 */
export type MiddleView = "new" | "interaction";

/** 右侧面板。 */
export type SidePanel = "workspace" | "diff" | "details" | "activities" | "plan" | "terminal";

/** 仅普通会话有的右侧面板（docs/PRD.md「主页面」）。 */
const SESSION_ONLY_PANELS: readonly SidePanel[] = ["workspace", "diff", "plan", "terminal"];

/** 右侧面板是否适用于该会话：普通会话专属面板在工作流会话下不展示。 */
export function panelAvailable(panel: SidePanel, target: OpenTarget): boolean {
  return target.kind === "session" || !SESSION_ONLY_PANELS.includes(panel);
}

export type Notice = { kind: "success" | "error"; text: string };

/** 待发送附件（拖拽或粘贴得到）。 */
export type Attachment = { block: ContentBlock; label: string };

/** 新建会话视图状态（docs/PRD.md「新建会话视图」）。 */
export type NewSessionState = {
  mode: "normal" | "workflow";
  /** 选中的机器名；空串 = 未选 */
  machine: string;
  /** 选中的 agent 名；空串 = 未选 */
  agent: string;
  workspace: string;
  useWorktree: boolean;
  /** 工作流模式：计划内容（手输或选择已保存计划） */
  plan: string;
  /** 工作流模式：选中的已保存计划名 */
  selectedPlan: string | null;
  /** 工作目录前缀匹配候选 */
  suggestions: FsEntry[];
  suggestionDir: string | null;
  suggestionPrefix: string;
  /** 是否展开最近工作目录列表 */
  recentOpen: boolean;
};

/** 设置浮窗分类。 */
export type SettingsTab =
  | "connection"
  | "machines"
  | "orchestrator"
  | "quickCommands"
  | "skills"
  | "plans";

/**
 * 编排智能体配置的读取状态：只有 `ready` 才表示配置已确认
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
  quickCommands: QuickCommand[];
  skills: Skill[];
  plans: WorkflowPlanItem[];
};

/** 终端输出增量（供 xterm.js 写入）。 */
export type TerminalChunk = { seq: number; bytes: Uint8Array; reset: boolean };

/** 打开会话的明细数据（普通会话与工作流会话共用）。 */
export type DetailState = {
  session: Session | null;
  workflow: Workflow | null;
  history: HistoryItem[];
  historyPaging: Paging;
  activities: Activity[];
  activitiesPaging: Paging;
  plan: SessionPlanEntry[];
  configOptions: SessionConfigOption[];
  slashCommands: SlashCommand[];
  ongoing: Activity | null;
  contextSize: number;
  contextWindowSize: number;
  terminals: Terminal[];
  activeTerminal: string | null;
  stream: TerminalStream;
  chunk: TerminalChunk;
};

export type CoreState = {
  status: ConnectionStatus;
  /** 连接错误（本地无 token 且未尝试登录时为 null，登录页据此不展示提示） */
  error: string | null;
  notice: Notice | null;
  entries: ListEntry[];
  listPaging: Paging;
  /** 展开的工作流会话 id */
  expanded: string[];
  middle: MiddleView;
  open: OpenTarget | null;
  sidePanel: SidePanel | null;
  detail: DetailState;
  /** 会话交互视图输入框草稿（改动审查引用会往这里追加） */
  inputDraft: string;
  newSession: NewSessionState;
  settings: SettingsState;
  recentWorkspaces: { machine: string; workspace: string; lastUsed: number }[];
  attachments: Attachment[];
};

export function initialDetail(): DetailState {
  return {
    session: null,
    workflow: null,
    history: [],
    historyPaging: newPaging(),
    activities: [],
    activitiesPaging: newPaging(),
    plan: [],
    configOptions: [],
    slashCommands: [],
    ongoing: null,
    contextSize: 0,
    contextWindowSize: 0,
    terminals: [],
    activeTerminal: null,
    stream: newTerminalStream(),
    chunk: { seq: 0, bytes: new Uint8Array(0), reset: false },
  };
}

export function initialNewSession(): NewSessionState {
  return {
    mode: "normal",
    machine: "",
    agent: "",
    workspace: "",
    useWorktree: false,
    plan: "",
    selectedPlan: null,
    suggestions: [],
    suggestionDir: null,
    suggestionPrefix: "",
    recentOpen: false,
  };
}

export function initialSettings(): SettingsState {
  return {
    open: false,
    tab: "connection",
    machines: [],
    agents: [],
    orchestrator: { status: "loading" },
    quickCommands: [],
    skills: [],
    plans: [],
  };
}

export function initialState(): CoreState {
  return {
    status: "connecting",
    error: null,
    notice: null,
    entries: [],
    listPaging: newPaging(),
    expanded: [],
    middle: "new",
    open: null,
    sidePanel: null,
    detail: initialDetail(),
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
  history?: number;
  ongoing?: number;
  activities?: number;
  plan?: number;
  options?: number;
  terminal?: number;
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
    this.state.detail = initialDetail();
  }

  /** 清空所有定时刷新节拍，使下次 tick 立即拉取。 */
  resetTicks(): void {
    this.last = {};
  }
}
