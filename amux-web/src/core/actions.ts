// 用户动作：调用 API、更新状态、触发对应视图的主动刷新（docs/DESIGN.md 各刷新机制）。

import { ApiClient, ApiError } from "../lib/api";
import { matchingPrefix, splitDirQuery } from "../lib/workspace";
import { encodeBase64 } from "../lib/terminal";
import { clearToken, loadToken, saveToken } from "../lib/token";
import type {
  Agent,
  ContentBlock,
  FsEntry,
  ListEntry,
  OpenTarget,
  OrchestratorConfig,
  QuickCommand,
  Session,
  Skill,
  WorkflowPlanItem,
} from "../lib/types";
import { entryTitle, rootDir } from "../lib/types";
import { initialNewSession, panelAvailable } from "./core";
import type { Attachment, Core, SidePanel } from "./core";
import {
  refreshHistory,
  refreshInteraction,
  refreshList,
  refreshNewSession,
  refreshSettings,
  refreshTerminalList,
  tick,
} from "./poll";

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** 已认证客户端：任一请求返回 401 时统一触发登录失效处理。 */
function authenticatedClient(core: Core, token: string): ApiClient {
  return new ApiClient("", token, undefined, (client) => core.invalidateAuthentication(client));
}

/** 启动：用本地 token 建立连接；本地无 token 时进入登录页面且不展示错误。 */
export async function start(core: Core): Promise<void> {
  const token = loadToken();
  if (token === "") {
    core.update((state) => {
      state.status = "offline";
      state.error = null;
    });
    return;
  }
  await connect(core, token, false);
}

/**
 * 建立连接：成功则保存 token 并进入主页面，失败则回到登录页面并展示错误。
 *
 * Web 端固定同源访问 Server（PRD「登录页面」：Web 应用无 Server 地址输入框）。
 */
async function connect(core: Core, token: string, notify: boolean): Promise<boolean> {
  core.update((state) => {
    state.status = "connecting";
    state.error = null;
  });
  const client = new ApiClient("", token);
  try {
    await client.ping();
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) clearToken();
    core.update((state) => {
      state.status = "failed";
      state.error = messageOf(error);
    });
    if (notify) core.failure(`连接失败：${messageOf(error)}`);
    return false;
  }
  core.client = authenticatedClient(core, token);
  saveToken(token);
  core.update((state) => {
    state.status = "online";
    state.error = null;
  });
  core.resetTicks();
  await Promise.all([refreshList(core), refreshNewSession(core)]);
  return true;
}

/** 登录页提交 token。 */
export function login(core: Core, token: string): Promise<boolean> {
  return connect(core, token, true);
}

/** 连接设置保存（PRD「连接设置」：保存结果以通知提示）。 */
export async function saveConnection(core: Core, token: string): Promise<void> {
  const client = new ApiClient("", token);
  try {
    await client.ping();
  } catch (error) {
    core.failure(`保存失败：${messageOf(error)}`);
    return;
  }
  core.client = authenticatedClient(core, token);
  saveToken(token);
  core.update((state) => {
    state.status = "online";
    state.error = null;
  });
  core.resetTicks();
  core.success("连接设置已保存");
  await refreshList(core);
}

// ---------- 会话列表与打开 ----------

export function showNewSession(core: Core): void {
  core.update((state) => {
    state.middle = "new";
    state.sidePanel = null;
    state.attachments = [];
  });
  void refreshNewSession(core);
}

export function toggleExpand(core: Core, workflowId: string): void {
  core.update((state) => {
    state.expanded = state.expanded.includes(workflowId)
      ? state.expanded.filter((id) => id !== workflowId)
      : [...state.expanded, workflowId];
  });
}

/** 打开会话：切换中间面板，立即拉取一次交互视图数据（DESIGN「会话交互视图」）。 */
export async function openEntry(core: Core, entry: ListEntry): Promise<void> {
  const target: OpenTarget =
    entry.kind === "session"
      ? { kind: "session", id: entry.session.id }
      : { kind: "workflow", id: entry.workflow.id };
  core.resetDetail();
  core.update((state) => {
    state.open = target;
    state.middle = "interaction";
    state.attachments = [];
    // 工作目录/改动审查/计划/终端仅普通会话有：切到不适用的会话时关闭面板
    // （docs/PRD.md「主页面」；否则会留下关闭按钮都已隐藏的空白面板）
    if (state.sidePanel !== null && !panelAvailable(state.sidePanel, target)) {
      state.sidePanel = null;
    }
    if (entry.kind === "session") {
      state.detail.session = entry.session;
    } else {
      state.detail.workflow = entry.workflow;
    }
  });
  core.resetTicks();
  await Promise.all([refreshInteraction(core), tick(core)]);
}

/** 切换右侧面板（再次点击收起）。 */
export function toggleSidePanel(core: Core, panel: SidePanel): void {
  let opened = false;
  core.update((state) => {
    const next = state.sidePanel === panel ? null : panel;
    state.sidePanel = next;
    opened = next === "terminal";
    if (next === "terminal") {
      state.detail.terminalSeq += 1;
      state.detail.terminalChunks = [
        ...state.detail.terminalChunks,
        { seq: state.detail.terminalSeq, bytes: new Uint8Array(0), reset: true },
      ];
    }
  });
  core.resetTicks();
  core.last.list = Date.now();
  // 终端视图打开时从 Server 拉取一次终端列表（docs/DESIGN.md「终端视图」）
  if (opened) void refreshTerminalList(core);
}

/** 删除会话（工作流会话连同关联普通会话，由 Server 级联）。 */
export async function deleteEntry(core: Core, entry: ListEntry): Promise<void> {
  if (!core.client) return;
  try {
    if (entry.kind === "session") {
      await core.client.deleteSession(entry.session.id);
    } else {
      await core.client.deleteWorkflow(entry.workflow.id);
    }
    if (core.state.open?.id === (entry.kind === "session" ? entry.session.id : entry.workflow.id)) {
      core.update((state) => {
        state.open = null;
      });
      showNewSession(core);
    }
    core.resetDetail();
    core.success(`已删除「${entryTitle(entry) || "未命名会话"}」`);
    await refreshList(core);
  } catch (error) {
    core.failure(`删除失败：${messageOf(error)}`);
  }
}

/** 重命名会话（标题由 Server 落盘）。 */
export async function renameEntry(core: Core, entry: ListEntry, title: string): Promise<void> {
  if (!core.client) return;
  try {
    if (entry.kind === "session") {
      await core.client.configureSession(entry.session.id, title, null);
    } else {
      await core.client.configureWorkflow(entry.workflow.id, title);
    }
    core.success("标题已更新");
    await refreshList(core);
  } catch (error) {
    core.failure(`重命名失败：${messageOf(error)}`);
  }
}

// ---------- 新建会话 ----------

/** 删除最近工作目录项：从列表移除并全量保存（docs/PRD.md「新建会话视图」删除按钮「x」）。 */
export async function deleteRecentWorkspace(
  core: Core,
  machine: string,
  workspace: string,
): Promise<void> {
  if (!core.client) return;
  const next = core.state.recentWorkspaces.filter(
    (item) => !(item.machine === machine && item.workspace === workspace),
  );
  try {
    await core.client.setRecentWorkspaces(next);
    core.update((state) => {
      state.recentWorkspaces = next;
    });
    core.success("已删除最近工作目录");
  } catch (error) {
    core.failure(`删除最近工作目录失败：${messageOf(error)}`);
  }
}

/** 普通模式创建会话。 */
export async function createSession(core: Core): Promise<void> {
  if (!core.client) return;
  const { machine, agent, workspace, useWorktree } = core.state.newSession;
  try {
    const session = await core.client.createSession({
      machine,
      agent,
      workspace,
      useWorktree,
    });
    core.update((state) => {
      state.newSession = { ...initialNewSession(), mode: state.newSession.mode };
    });
    await refreshList(core);
    await openEntry(core, { kind: "session", session });
  } catch (error) {
    core.failure(`创建会话失败：${messageOf(error)}`);
  }
}

/** 工作流模式创建会话。 */
export async function createWorkflow(core: Core): Promise<void> {
  if (!core.client) return;
  const plan = core.state.newSession.plan.trim();
  if (plan === "") return;
  try {
    const workflow = await core.client.createWorkflow(plan, null);
    core.update((state) => {
      state.newSession = { ...initialNewSession(), mode: state.newSession.mode };
    });
    await refreshList(core);
    await openEntry(core, { kind: "workflow", workflow });
  } catch (error) {
    core.failure(`创建工作流会话失败：${messageOf(error)}`);
  }
}

/**
 * 工作目录输入：拆分为（目录, 前缀）。
 *
 * 目录变了（输入跨过目录边界，如 `/`、`/home/`、`/home/tom/`）才拉取该目录的全部目录项；
 * 目录没变时直接用已拉取的条目做前缀匹配，不为同一个目录重复拉取
 * （docs/DESIGN.md「新建会话视图」）。
 */
export async function updateWorkspaceInput(core: Core, text: string): Promise<void> {
  const query = splitDirQuery(text);
  const machine = core.state.newSession.machine;
  if (query === null || machine === "" || !core.client) {
    core.update((state) => {
      state.newSession.suggestions = [];
      state.newSession.suggestion = null;
      state.newSession.suggestionPrefix = "";
    });
    return;
  }
  const listed = core.state.newSession.suggestion;
  if (listed !== null && listed.machine === machine && listed.dir === query.dir) {
    core.update((state) => {
      state.newSession.suggestionPrefix = query.prefix;
      state.newSession.suggestions = matchingPrefix(listed.entries, query.prefix);
    });
    return;
  }
  core.update((state) => {
    // 先占位：新目录的条目到达前不展示上一个目录的项，也不为它重复拉取
    state.newSession.suggestion = { machine, dir: query.dir, entries: [] };
    state.newSession.suggestionPrefix = query.prefix;
    state.newSession.suggestions = [];
  });
  const client = core.client;
  // 前缀匹配要在完整列表上做，因此按分页续拉，直到没有更多
  const entries: FsEntry[] = [];
  try {
    let offset = 0;
    while (true) {
      const result = await client.listDir(machine, query.dir, undefined, offset, true);
      entries.push(...result.entries);
      if (!result.hasMore) break;
      const next = result.nextOffset;
      if (next <= offset) break;
      offset = next;
    }
  } catch {
    core.update((state) => {
      // 撤掉占位，下次输入时重试
      const listing = state.newSession.suggestion;
      if (listing === null || listing.machine !== machine || listing.dir !== query.dir) return;
      state.newSession.suggestion = null;
      state.newSession.suggestions = [];
    });
    return;
  }
  core.update((state) => {
    // 用户可能已经继续输入、换了目录或换了机器：过期应答不落地
    const listing = state.newSession.suggestion;
    if (listing === null || listing.machine !== machine || listing.dir !== query.dir) return;
    state.newSession.suggestion = { machine, dir: query.dir, entries };
    state.newSession.suggestions = matchingPrefix(entries, state.newSession.suggestionPrefix);
  });
}

// ---------- 会话交互 ----------

/** 附件：图片按 blob（base64）发送，文本文件按 text 发送；uri 保留文件名供历史展示。 */
export async function attachmentFromFile(file: File): Promise<Attachment> {
  if (file.type.startsWith("image/")) {
    const bytes = new Uint8Array(await file.arrayBuffer());
    return {
      block: { type: "resource", mimeType: file.type, uri: file.name, blob: encodeBase64(bytes) },
      label: file.name,
    };
  }
  return {
    block: {
      type: "resource",
      mimeType: file.type || "text/plain",
      uri: file.name,
      text: await file.text(),
    },
    label: file.name,
  };
}

export async function addFiles(core: Core, files: FileList | File[]): Promise<void> {
  const attachments = await Promise.all([...files].map(attachmentFromFile));
  core.update((state) => {
    state.attachments = [...state.attachments, ...attachments];
  });
}

export function removeAttachment(core: Core, index: number): void {
  core.update((state) => {
    state.attachments = state.attachments.filter((_, at) => at !== index);
  });
}

/** 仅输入框发送会消费待发送附件。 */
export async function sendPrompt(
  core: Core,
  text: string,
  includeAttachments = true,
): Promise<boolean> {
  const target = core.state.open;
  if (!core.client || !target) return false;
  const input: ContentBlock[] = [];
  if (text !== "") input.push({ type: "text", text });
  if (includeAttachments) {
    input.push(...core.state.attachments.map((attachment) => attachment.block));
  }
  if (input.length === 0) return false;
  try {
    if (target.kind === "session") {
      await core.client.promptSession(target.id, input);
    } else {
      await core.client.promptWorkflow(target.id, input);
    }
  } catch (error) {
    core.failure(`发送失败：${messageOf(error)}`);
    return false;
  }
  core.update((state) => {
    if (includeAttachments) state.attachments = [];
  });
  await Promise.all([refreshHistory(core), refreshList(core)]);
  core.last.history = Date.now();
  core.last.list = Date.now();
  return true;
}

/**
 * 取消进行中的工作；工作流会话以用户消息方式取消（PRD「工作流会话取消」），
 * 该消息同样触发会话列表主动刷新（docs/DESIGN.md「会话列表刷新机制」）。
 */
export async function cancelOpen(core: Core): Promise<void> {
  const target = core.state.open;
  if (!core.client || !target) return;
  try {
    if (target.kind === "session") {
      await core.client.cancelSession(target.id);
    } else {
      await core.client.promptWorkflow(target.id, [
        { type: "text", text: "取消当前进行中的全部工作" },
      ]);
      await refreshList(core);
      core.last.list = Date.now();
    }
    await refreshHistory(core);
  } catch (error) {
    core.failure(`取消失败：${messageOf(error)}`);
  }
}

/** 会话选项：设置后由 Server 全量覆盖并缓存。 */
export async function setConfigOption(
  core: Core,
  configId: string,
  value: { type: "value_id"; value: string } | { type: "boolean"; value: boolean },
): Promise<void> {
  const target = core.state.open;
  if (!core.client || !target || target.kind !== "session") return;
  try {
    await core.client.configureSession(target.id, null, { configId, ...value });
    const options = await core.client.configOptions(target.id);
    core.update((state) => {
      state.detail.configOptions = options;
    });
  } catch (error) {
    core.failure(`设置会话选项失败：${messageOf(error)}`);
  }
}

// ---------- 终端 ----------

/** 打开终端：以会话工作目录为 cwd，返回的终端立即成为活动终端。 */
export async function openTerminal(core: Core, cols: number, rows: number): Promise<void> {
  const target = core.state.open;
  if (!core.client || !target || target.kind !== "session") return;
  const session = core.state.detail.session;
  try {
    const terminalId = await core.client.openTerminal(
      target.id,
      session ? rootDir(session) : null,
      cols,
      rows,
    );
    const terminals = await core.client.terminals(target.id);
    core.update((state) => {
      state.detail.terminals = terminals;
      state.detail.activeTerminal = terminalId;
      state.detail.terminalSeq += 1;
      state.detail.terminalChunks = [
        ...state.detail.terminalChunks,
        { seq: state.detail.terminalSeq, bytes: new Uint8Array(0), reset: true },
      ];
    });
  } catch (error) {
    core.failure(`打开终端失败：${messageOf(error)}`);
  }
}

export function selectTerminal(core: Core, terminalId: string): void {
  core.update((state) => {
    state.detail.activeTerminal = terminalId;
    state.detail.terminalSeq += 1;
    state.detail.terminalChunks = [
      ...state.detail.terminalChunks,
      { seq: state.detail.terminalSeq, bytes: new Uint8Array(0), reset: true },
    ];
  });
  core.resetTicks();
}

export async function closeTerminal(core: Core, terminalId: string): Promise<void> {
  const target = core.state.open;
  if (!core.client || !target || target.kind !== "session") return;
  try {
    await core.client.closeTerminal(target.id, terminalId);
    const terminals = await core.client.terminals(target.id);
    core.update((state) => {
      state.detail.terminals = terminals;
      if (state.detail.activeTerminal === terminalId) {
        state.detail.activeTerminal = terminals.at(-1)?.id ?? null;
        state.detail.terminalSeq += 1;
        state.detail.terminalChunks = [
          ...state.detail.terminalChunks,
          { seq: state.detail.terminalSeq, bytes: new Uint8Array(0), reset: true },
        ];
      }
    });
  } catch (error) {
    core.failure(`关闭终端失败：${messageOf(error)}`);
  }
}

export async function sendTerminalInput(core: Core, bytes: Uint8Array): Promise<void> {
  const target = core.state.open;
  const terminal = core.state.detail.activeTerminal;
  if (!core.client || !target || target.kind !== "session" || terminal === null) return;
  try {
    await core.client.terminalInput(target.id, terminal, encodeBase64(bytes));
  } catch (error) {
    core.failure(`终端输入失败：${messageOf(error)}`);
  }
}

export async function resizeTerminal(core: Core, cols: number, rows: number): Promise<void> {
  const target = core.state.open;
  const terminal = core.state.detail.activeTerminal;
  if (!core.client || !target || target.kind !== "session" || terminal === null) return;
  try {
    await core.client.resizeTerminal(target.id, terminal, cols, rows);
  } catch {
    // 窗口尺寸调整失败不影响交互，下一周期会随输出刷新
  }
}

// ---------- 设置 ----------

export function openSettings(core: Core, tab?: Core["state"]["settings"]["tab"]): void {
  core.update((state) => {
    state.settings.open = true;
    if (tab) state.settings.tab = tab;
  });
  const current = core.state.settings.tab;
  void refreshSettings(core, current);
}

export function closeSettings(core: Core): void {
  core.update((state) => {
    state.settings.open = false;
  });
}

export function selectSettingsTab(core: Core, tab: Core["state"]["settings"]["tab"]): void {
  core.update((state) => {
    state.settings.tab = tab;
  });
  void refreshSettings(core, tab);
}

/** 保存内置智能体配置（PRD「内置智能体设置」）。 */
export async function saveOrchestrator(core: Core, config: OrchestratorConfig): Promise<void> {
  if (!core.client) return;
  try {
    await core.client.setOrchestrator(config);
    core.update((state) => {
      state.settings.orchestrator = { status: "ready", config };
    });
    core.success("内置智能体配置已保存");
  } catch (error) {
    core.failure(`保存失败：${messageOf(error)}`);
  }
}

export async function saveQuickCommands(core: Core, commands: QuickCommand[]): Promise<void> {
  if (!core.client) return;
  try {
    await core.client.setQuickCommands(commands);
    core.update((state) => {
      state.settings.quickCommands = commands;
    });
    core.success("快捷指令已保存");
  } catch (error) {
    core.failure(`保存失败：${messageOf(error)}`);
  }
}

export async function saveSkills(core: Core, skills: Skill[]): Promise<void> {
  if (!core.client) return;
  try {
    await core.client.setSkills(skills);
    core.update((state) => {
      state.settings.skills = skills;
    });
    core.success("技能已保存");
  } catch (error) {
    core.failure(`保存失败：${messageOf(error)}`);
  }
}

export async function savePlans(core: Core, plans: WorkflowPlanItem[]): Promise<void> {
  if (!core.client) return;
  try {
    await core.client.setWorkflowPlans(plans);
    core.update((state) => {
      state.settings.plans = plans;
    });
    core.success("工作流计划已保存");
  } catch (error) {
    core.failure(`保存失败：${messageOf(error)}`);
  }
}

/** 重新发现机器上的 agents。 */
export async function rediscover(core: Core, machine: string): Promise<void> {
  if (!core.client) return;
  try {
    const agents = await core.client.rediscover(machine);
    core.update((state) => {
      state.settings.agents = state.settings.agents.map((entry) =>
        entry.machine === machine ? { machine, agents } : entry,
      );
    });
    core.success(`已重新发现 ${machine} 的 agents`);
  } catch (error) {
    core.failure(`重新发现失败：${messageOf(error)}`);
  }
}

/** 重启指定机器上的 agent。 */
export async function restartAgent(core: Core, machine: string, agent: string): Promise<void> {
  if (!core.client) return;
  try {
    await core.client.restartAgent(machine, agent);
    const agents: Agent[] = await core.client.agents(machine).catch(() => []);
    core.update((state) => {
      state.settings.agents = state.settings.agents.map((entry) =>
        entry.machine === machine ? { machine, agents } : entry,
      );
    });
    core.success(`已重启 ${agent}@${machine}`);
  } catch (error) {
    core.failure(`重启失败：${messageOf(error)}`);
  }
}

/**
 * 技能安装/更新/卸载：以机器临时目录为工作目录创建普通会话并发送指令
 * （docs/DESIGN.md「技能操作」），随后进入该会话供用户随时干预。
 */
export async function runSkillAction(
  core: Core,
  skill: Skill,
  action: "安装" | "更新" | "卸载",
  machine: string,
  agent: string,
): Promise<void> {
  if (!core.client) return;
  let tempDir: string;
  try {
    const machines = await core.client.machines();
    const target = machines.find((item) => item.name === machine);
    if (!target) {
      core.failure(`找不到机器 ${machine}`);
      return;
    }
    tempDir = target.tempDir;
  } catch (error) {
    core.failure(`读取机器信息失败：${messageOf(error)}`);
    return;
  }
  let session: Session;
  try {
    session = await core.client.createSession({ machine, agent, workspace: tempDir, useWorktree: false });
  } catch (error) {
    core.failure(`创建技能会话失败：${messageOf(error)}`);
    return;
  }
  const prompt = `以下是技能 ${skill.name} 的描述，请${action}此技能\n> ${skill.description}`;
  try {
    await core.client.promptSession(session.id, [{ type: "text", text: prompt }]);
  } catch (error) {
    core.failure(`发送技能指令失败：${messageOf(error)}`);
    return;
  }
  core.success(`已发起技能${action}会话`);
  await refreshList(core);
  await openEntry(core, { kind: "session", session });
}
