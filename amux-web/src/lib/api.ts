// Client API 客户端：经 HTTPS 调用 Server（docs/DESIGN.md「Client-Server 通信」）。
//
// Web 端与 Server 同源，`baseUrl` 缺省为空串（相对路径请求）。

import type {
  ActivitiesPage,
  Agent,
  ConfigOptions,
  ConfigureSessionRequest,
  ContextInfo,
  CreateSessionRequest,
  CreateWorkflowRequest,
  ConfigureWorkflowRequest,
  FsListResult,
  FsReadResult,
  GitDiffResult,
  HistoryPage,
  Machine,
  OngoingActivity,
  OpAck,
  OrchestratorConfig,
  Plan,
  PromptRequest,
  QuickCommand,
  RecentWorkspace,
  Session,
  SessionConfigOption,
  SessionConfigSetting,
  SessionList,
  SlashCommand,
  Skill,
  Terminal,
  TerminalOutput,
  Workflow,
  WorkflowList,
  WorkflowPlanItem,
} from "./types";
import { createSseDecoder } from "./sse";

/** 会话页大小上限（服务端也会夹取）。 */
const FS_PAGE_LIMIT = 500;
/** 服务端分页上限（`Page::limit` 夹取到 500）。 */
const PAGE_LIMIT_MAX = 500;

/** 非 2xx 的错误消息：与桌面应用的 `Client` 保持一致（`HTTP <status>: <body>`）。 */
export function errorMessage(status: number, body: string): string {
  const trimmed = body.trim();
  return trimmed === "" ? `HTTP ${status}` : `HTTP ${status}: ${trimmed}`;
}

/** 保留 HTTP 状态码的 API 错误，供跨请求的统一认证处理使用。 */
export class ApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

export class ApiClient {
  private unauthorized = false;

  constructor(
    private readonly baseUrl: string,
    private readonly token: string,
    private readonly fetchFn: typeof fetch = globalThis.fetch.bind(globalThis),
    private readonly onUnauthorized?: (client: ApiClient) => void,
  ) {}

  get authToken(): string {
    return this.token;
  }

  // ---------- 机器与 agent ----------

  /** 连通性与认证检查（`GET /machines`）。 */
  async ping(): Promise<void> {
    await this.machines();
  }

  machines(): Promise<Machine[]> {
    return this.getJson("/machines");
  }

  agents(machine: string): Promise<Agent[]> {
    return this.getJson(`/machines/${encodeURIComponent(machine)}/agents`);
  }

  rediscover(machine: string): Promise<Agent[]> {
    return this.postJson(`/machines/${encodeURIComponent(machine)}/agents/rediscover`, {});
  }

  restartAgent(machine: string, agent: string): Promise<void> {
    return this.postEmpty(
      `/machines/${encodeURIComponent(machine)}/agents/${encodeURIComponent(agent)}/restart`,
      {},
    );
  }

  /** 列目录；`dirsOnly` 只返回子目录（工作目录前缀匹配用）。 */
  listDir(
    machine: string,
    path: string,
    limit: number = FS_PAGE_LIMIT,
    offset = 0,
    dirsOnly = false,
  ): Promise<FsListResult> {
    return this.getJson(
      `/machines/${encodeURIComponent(machine)}/list_dir?${query({ path, limit, offset, dirs_only: dirsOnly })}`,
    );
  }

  readFile(machine: string, path: string, limit: number, offset: number): Promise<FsReadResult> {
    return this.getJson(
      `/machines/${encodeURIComponent(machine)}/read_file?${query({ path, limit, offset })}`,
    );
  }

  // ---------- 普通会话 ----------

  createSession(request: CreateSessionRequest): Promise<Session> {
    return this.postJson("/sessions", request);
  }

  sessions(limit: number, offset: number): Promise<SessionList> {
    return this.getJson(`/sessions?${query({ limit, offset })}`);
  }

  session(id: string): Promise<Session> {
    return this.getJson(`/sessions/${encodeURIComponent(id)}`);
  }

  promptSession(id: string, input: PromptRequest["input"]): Promise<void> {
    return this.postEmpty(`/sessions/${encodeURIComponent(id)}`, { input });
  }

  cancelSession(id: string): Promise<void> {
    return this.postEmpty(`/sessions/${encodeURIComponent(id)}/cancel`, {});
  }

  deleteSession(id: string): Promise<void> {
    return this.delete(`/sessions/${encodeURIComponent(id)}`);
  }

  configureSession(
    id: string,
    title: string | null,
    config: SessionConfigSetting | null,
  ): Promise<void> {
    const body: ConfigureSessionRequest = {};
    if (title !== null) body.title = title;
    if (config !== null) body.config = config;
    return this.postEmpty(`/sessions/${encodeURIComponent(id)}/configure`, body);
  }

  async configOptions(id: string): Promise<SessionConfigOption[]> {
    const response = await this.getJson<ConfigOptions>(
      `/sessions/${encodeURIComponent(id)}/config_options`,
    );
    return response.options;
  }

  async slashCommands(id: string): Promise<SlashCommand[]> {
    const response = await this.getJson<{ commands: SlashCommand[] }>(
      `/sessions/${encodeURIComponent(id)}/slash_commands`,
    );
    return response.commands;
  }

  async plan(id: string): Promise<Plan["entries"]> {
    const response = await this.getJson<Plan>(`/sessions/${encodeURIComponent(id)}/plan`);
    return response.entries;
  }

  context(id: string): Promise<ContextInfo> {
    return this.getJson(`/sessions/${encodeURIComponent(id)}/context`);
  }

  history(id: string, limit: number, offset: number): Promise<HistoryPage> {
    return this.getJson(
      `/sessions/${encodeURIComponent(id)}/history?${query({ limit: clamp(limit), offset })}`,
    );
  }

  activities(id: string, limit: number, offset: number): Promise<ActivitiesPage> {
    return this.getJson(
      `/sessions/${encodeURIComponent(id)}/activities?${query({ limit: clamp(limit), offset })}`,
    );
  }

  async ongoingActivity(id: string) {
    const response = await this.getJson<OngoingActivity>(
      `/sessions/${encodeURIComponent(id)}/ongoing_activity`,
    );
    return response.activity ?? null;
  }

  diff(id: string): Promise<GitDiffResult> {
    return this.getJson(`/sessions/${encodeURIComponent(id)}/diff`);
  }

  // ---------- 终端 ----------

  async openTerminal(id: string, cwd: string | null, cols: number, rows: number): Promise<string> {
    const response = await this.postJson<{ terminalId: string }>(
      `/sessions/${encodeURIComponent(id)}/terminals`,
      { cwd, cols, rows },
    );
    return response.terminalId;
  }

  terminals(id: string): Promise<Terminal[]> {
    return this.getJson(`/sessions/${encodeURIComponent(id)}/terminals`);
  }

  terminalInput(id: string, terminal: string, data: string): Promise<void> {
    return this.postEmpty(this.terminalPath(id, terminal), { data });
  }

  async terminalOutputStream(
    id: string,
    terminal: string,
    signal: AbortSignal,
    onOutput: (output: TerminalOutput) => void,
  ): Promise<void> {
    const response = await this.send(this.terminalPath(id, terminal), { method: "GET", signal });
    if (!response.ok) {
      const body = await response.text().catch(() => "");
      throw new ApiError(response.status, errorMessage(response.status, body));
    }
    if (response.body === null) throw new Error("终端流响应缺少 body");

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    const events = createSseDecoder();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        for (const data of events.push(decoder.decode(value, { stream: true }))) {
          onOutput(JSON.parse(data) as TerminalOutput);
        }
      }
    } finally {
      reader.releaseLock();
    }
  }

  closeTerminal(id: string, terminal: string): Promise<void> {
    return this.delete(this.terminalPath(id, terminal));
  }

  resizeTerminal(id: string, terminal: string, cols: number, rows: number): Promise<void> {
    return this.postEmpty(`${this.terminalPath(id, terminal)}/resize`, { cols, rows });
  }

  private terminalPath(id: string, terminal: string): string {
    return `/sessions/${encodeURIComponent(id)}/terminals/${encodeURIComponent(terminal)}`;
  }

  // ---------- 工作流会话 ----------

  createWorkflow(plan: string, title: string | null): Promise<Workflow> {
    const body: CreateWorkflowRequest = title === null ? { plan } : { plan, title };
    return this.postJson("/workflows", body);
  }

  workflows(limit: number, offset: number): Promise<WorkflowList> {
    return this.getJson(`/workflows?${query({ limit, offset })}`);
  }

  workflow(id: string): Promise<Workflow> {
    return this.getJson(`/workflows/${encodeURIComponent(id)}`);
  }

  promptWorkflow(id: string, input: PromptRequest["input"]): Promise<void> {
    return this.postEmpty(`/workflows/${encodeURIComponent(id)}`, { input });
  }

  deleteWorkflow(id: string): Promise<void> {
    return this.delete(`/workflows/${encodeURIComponent(id)}`);
  }

  configureWorkflow(id: string, title: string): Promise<void> {
    const body: ConfigureWorkflowRequest = { title };
    return this.postEmpty(`/workflows/${encodeURIComponent(id)}/configure`, body);
  }

  workflowHistory(id: string, limit: number, offset: number): Promise<HistoryPage> {
    return this.getJson(
      `/workflows/${encodeURIComponent(id)}/history?${query({ limit: clamp(limit), offset })}`,
    );
  }

  workflowActivities(id: string, limit: number, offset: number): Promise<ActivitiesPage> {
    return this.getJson(
      `/workflows/${encodeURIComponent(id)}/activities?${query({ limit: clamp(limit), offset })}`,
    );
  }

  async workflowOngoingActivity(id: string) {
    const response = await this.getJson<OngoingActivity>(
      `/workflows/${encodeURIComponent(id)}/ongoing_activity`,
    );
    return response.activity ?? null;
  }

  // ---------- 配置 ----------

  skills(): Promise<Skill[]> {
    return this.getJson("/config/skills/");
  }

  setSkills(skills: Skill[]): Promise<void> {
    return this.putEmpty("/config/skills/", skills);
  }

  workflowPlans(): Promise<WorkflowPlanItem[]> {
    return this.getJson("/config/workflows/");
  }

  setWorkflowPlans(plans: WorkflowPlanItem[]): Promise<void> {
    return this.putEmpty("/config/workflows/", plans);
  }

  recentWorkspaces(): Promise<RecentWorkspace[]> {
    return this.getJson("/config/recent_workspaces/");
  }

  quickCommands(): Promise<QuickCommand[]> {
    return this.getJson("/config/quick_commands/");
  }

  setQuickCommands(commands: QuickCommand[]): Promise<void> {
    return this.putEmpty("/config/quick_commands/", commands);
  }

  orchestrator(): Promise<OrchestratorConfig | null> {
    return this.getJson("/config/agent/");
  }

  setOrchestrator(config: OrchestratorConfig): Promise<void> {
    return this.putEmpty("/config/agent/", config);
  }

  // ---------- 内部 ----------

  private url(path: string): string {
    return `${this.baseUrl}${path}`;
  }

  private async send(path: string, init: RequestInit): Promise<Response> {
    const response = await this.fetchFn(this.url(path), {
      ...init,
      headers: {
        authorization: `Bearer ${this.token}`,
        ...(init.headers ?? {}),
      },
    });
    if (response.status === 401 && !this.unauthorized) {
      this.unauthorized = true;
      this.onUnauthorized?.(this);
    }
    return response;
  }

  private async getJson<T>(path: string): Promise<T> {
    return decode<T>(await this.send(path, { method: "GET" }));
  }

  private async postJson<T>(path: string, body: unknown): Promise<T> {
    return decode<T>(await this.sendJson(path, "POST", body));
  }

  private async postEmpty(path: string, body: unknown): Promise<void> {
    await decode<OpAck>(await this.sendJson(path, "POST", body));
  }

  private async putEmpty(path: string, body: unknown): Promise<void> {
    await decode<OpAck>(await this.sendJson(path, "PUT", body));
  }

  private sendJson(path: string, method: string, body: unknown): Promise<Response> {
    return this.send(path, {
      method,
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
  }

  private async delete(path: string): Promise<void> {
    await decode<OpAck>(await this.send(path, { method: "DELETE" }));
  }
}

async function decode<T>(response: Response): Promise<T> {
  if (!response.ok) {
    const body = await response.text().catch(() => "");
    throw new ApiError(response.status, errorMessage(response.status, body));
  }
  const text = await response.text();
  try {
    return JSON.parse(text) as T;
  } catch (error) {
    throw new Error(`响应解析失败: ${String(error)}`);
  }
}

function clamp(limit: number): number {
  return Math.max(1, Math.min(PAGE_LIMIT_MAX, limit));
}

/** 查询串：值为 false 的布尔项省略（服务端各处均为 `#[serde(default)]`）。 */
function query(params: Record<string, string | number | boolean>): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === false) continue;
    search.set(key, String(value));
  }
  return search.toString();
}
