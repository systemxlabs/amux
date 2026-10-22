// 用户动作：调用 API、更新状态、触发对应视图的主动刷新（docs/DESIGN.md 各刷新机制）。

import { ApiClient, ApiError } from "../lib/api";
import { encodeBase64 } from "../lib/terminal";
import { clearToken, loadToken, saveToken } from "../lib/token";
import type {
  Agent,
  Automation,
  AutomationConfig,
  AutomationInput,
  ContentBlock,
  GroupMemberTemplate,
  ListEntry,
  OpenTarget,
  OrchestratorConfig,
  Project,
  QuickCommand,
  Session,
  Skill,
  WorkflowPlanItem,
  WorktreeBase,
} from "../lib/types";
import { entryId, entryPinned, entryTitle, execDir, worktreeBaseFromBranch } from "../lib/types";
import { initialNewSession, panelAvailable } from "./core";
import type { Core, PendingAttachment, SidePanel } from "./core";
import {
  refreshHistory,
  refreshAutomations,
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

export async function saveAutomationConfig(
  core: Core,
  config: AutomationConfig,
): Promise<void> {
  if (!core.client) return;
  try {
    await core.client.setAutomationConfig(config);
    core.update((state) => {
      state.settings.automation = config;
      state.settings.automationLoaded = true;
    });
    core.success("自动化配置已保存");
  } catch (error) {
    core.failure(`保存自动化配置失败：${messageOf(error)}`);
  }
}

// ---------- 会话列表与打开 ----------

export function showNewSession(core: Core): void {
  core.update((state) => {
    state.middle = "new";
    state.sidePanel = null;
    state.automationPanel = null;
    for (const attachment of state.attachments) attachment.controller?.abort();
    state.attachments = [];
  });
  void refreshNewSession(core);
}

export function toggleExpand(core: Core, id: string): void {
  core.update((state) => {
    state.expanded = state.expanded.includes(id)
      ? state.expanded.filter((item) => item !== id)
      : [...state.expanded, id];
  });
}

/** 打开会话：切换中间面板，立即拉取一次交互视图数据（DESIGN「会话交互视图」）。 */
export async function openEntry(core: Core, entry: ListEntry): Promise<void> {
  const target: OpenTarget =
    entry.kind === "session"
      ? { kind: "session", id: entry.session.id }
      : entry.kind === "workflow"
        ? { kind: "workflow", id: entry.workflow.id }
        : { kind: "group", id: entry.group.id };
  core.resetDetail();
  core.update((state) => {
    state.open = target;
    state.middle = "interaction";
    state.automationPanel = null;
    for (const attachment of state.attachments) attachment.controller?.abort();
    state.attachments = [];
    // 执行目录/改动审查/计划/终端仅普通会话有：切到不适用的会话时关闭面板
    // （docs/PRD.md「主页面」；否则会留下关闭按钮都已隐藏的空白面板）
    if (state.sidePanel !== null && !panelAvailable(state.sidePanel, target)) {
      state.sidePanel = null;
    }
    if (entry.kind === "session") {
      state.detail.session = entry.session;
    } else if (entry.kind === "workflow") {
      state.detail.workflow = entry.workflow;
    } else {
      state.detail.group = entry.group;
    }
  });
  core.resetTicks();
  await Promise.all([refreshInteraction(core), tick(core)]);
}

/** 打开自动化列表，并清空当前会话视图状态。 */
export async function showAutomations(core: Core): Promise<void> {
  core.update((state) => {
    state.middle = "automation";
    state.open = null;
    state.sidePanel = null;
    state.automationPanel = null;
  });
  core.resetTicks();
  await Promise.all([refreshAutomations(core), refreshList(core)]);
}

export function showNewAutomation(core: Core): void {
  core.update((state) => {
    state.middle = "automation";
    state.open = null;
    state.sidePanel = null;
    state.automationPanel = { kind: "new" };
  });
}

export function showAutomationDetail(core: Core, id: string): void {
  core.update((state) => {
    state.middle = "automation";
    state.open = null;
    state.sidePanel = null;
    state.automationPanel = { kind: "detail", id };
  });
}

export function showEditAutomation(core: Core, id: string): void {
  core.update((state) => {
    state.middle = "automation";
    state.open = null;
    state.sidePanel = null;
    state.automationPanel = { kind: "edit", id };
  });
}

export function closeAutomationPanel(core: Core): void {
  core.update((state) => {
    state.automationPanel = null;
  });
}

export async function saveAutomation(
  core: Core,
  input: AutomationInput,
  id?: string,
): Promise<boolean> {
  if (!core.client) return false;
  try {
    const automation =
      id === undefined
        ? await core.client.createAutomation(input)
        : await core.client.updateAutomation(id, input);
    core.update((state) => {
      const index = state.automations.findIndex((item) => item.id === automation.id);
      if (index === -1) state.automations = [automation, ...state.automations];
      else state.automations[index] = automation;
      state.automationPanel = { kind: "detail", id: automation.id };
    });
    core.success(id === undefined ? "自动化已创建" : "自动化已保存");
    await refreshAutomations(core);
    return true;
  } catch (error) {
    core.failure(`保存自动化失败：${messageOf(error)}`);
    return false;
  }
}

export async function deleteAutomation(core: Core, automation: Automation): Promise<void> {
  if (!core.client) return;
  try {
    await core.client.deleteAutomation(automation.id);
    core.update((state) => {
      state.automations = state.automations.filter((item) => item.id !== automation.id);
      if (
        state.automationPanel !== null &&
        state.automationPanel.kind !== "new" &&
        state.automationPanel.id === automation.id
      ) {
        state.automationPanel = null;
      }
    });
    core.success(`已删除「${automation.title}」`);
  } catch (error) {
    core.failure(`删除自动化失败：${messageOf(error)}`);
  }
}

export async function toggleAutomation(core: Core, automation: Automation): Promise<void> {
  await saveAutomation(
    core,
    {
      title: automation.title,
      enabled: !automation.enabled,
      task: automation.task,
      trigger: automation.trigger,
      sessionOptions: automation.sessionOptions,
      resultLabels: automation.resultLabels,
    },
    automation.id,
  );
}

export async function triggerAutomation(core: Core, automation: Automation): Promise<void> {
  if (!core.client) return;
  try {
    await core.client.triggerAutomation(automation.id);
    core.success(`已触发「${automation.title}」`);
    await refreshAutomations(core);
  } catch (error) {
    core.failure(`触发自动化失败：${messageOf(error)}`);
  }
}

/** 切换右侧面板（再次点击收起）。 */
export function toggleSidePanel(core: Core, panel: SidePanel): void {
  let opened = false;
  let openedPanel: SidePanel | null = null;
  core.update((state) => {
    const next = state.sidePanel === panel ? null : panel;
    state.sidePanel = next;
    opened = next === "terminal" || next === "attachments";
    openedPanel = next;
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
  if (opened && openedPanel === "terminal") void refreshTerminalList(core);
  if (opened && openedPanel === "attachments") void refreshAttachments(core);
}

/** 删除会话（工作流会话连同关联普通会话，由 Server 级联）。 */
export async function deleteEntry(core: Core, entry: ListEntry): Promise<void> {
  if (!core.client) return;
  try {
    if (entry.kind === "session") {
      await core.client.deleteSession(entry.session.id);
    } else if (entry.kind === "workflow") {
      await core.client.deleteWorkflow(entry.workflow.id);
    } else {
      await core.client.deleteGroup(entry.group.id);
    }
    if (core.state.open?.id === entryId(entry)) {
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
      await core.client.configureSession(
        entry.session.id,
        title,
        null,
        entry.session.project ?? null,
        entry.session.pinned,
      );
    } else if (entry.kind === "workflow") {
      await core.client.configureWorkflow(
        entry.workflow.id,
        title,
        entry.workflow.project ?? null,
        entry.workflow.pinned,
      );
    } else {
      await core.client.configureGroup(
        entry.group.id,
        title,
        entry.group.project ?? null,
        entry.group.pinned,
      );
    }
    core.success("标题已更新");
    await refreshList(core);
  } catch (error) {
    core.failure(`重命名失败：${messageOf(error)}`);
  }
}

/** 以列表项的相同设置快速新建并打开一个普通、工作流或群聊会话。 */
export async function createFromEntry(core: Core, entry: ListEntry): Promise<void> {
  if (!core.client) return;
  try {
    if (entry.kind === "session") {
      const useWorktree = entry.session.worktreeDir !== "";
      let worktreeBase: WorktreeBase | undefined;
      if (useWorktree) {
        const branch = (
          await core.client
            .listBranches(entry.session.machine, execDir(entry.session))
            .catch(() => ({ branches: [] }))
        ).branches.find((item) => item.isWorktreeBase);
        if (branch === undefined) {
          core.failure("无法识别 worktree 基分支");
          return;
        }
        worktreeBase = worktreeBaseFromBranch(branch);
      }
      const session = await core.client.createSession({
        machine: entry.session.machine,
        agent: entry.session.agent,
        workspace: entry.session.workspace,
        useWorktree,
        worktreeBase,
        project: entry.session.project,
      });
      await refreshList(core);
      await openEntry(core, { kind: "session", session });
    } else if (entry.kind === "workflow") {
      const workflow = await core.client.createWorkflow(
        entry.workflow.plan,
        null,
        entry.workflow.project,
      );
      await refreshList(core);
      await openEntry(core, { kind: "workflow", workflow });
    } else {
      const members: GroupMemberTemplate[] = [];
      for (const member of entry.group.members) {
        const session = entry.group.sessions.find((item) => item.id === member.sessionId);
        if (session === undefined) {
          core.failure(`无法读取群聊成员「${member.name}」的会话`);
          return;
        }
        const useWorktree = session.worktreeDir !== "";
        let worktreeBase: WorktreeBase | undefined;
        if (useWorktree) {
          const branch = (
            await core.client
              .listBranches(session.machine, execDir(session))
              .catch(() => ({ branches: [] }))
          ).branches.find((item) => item.isWorktreeBase);
          if (branch === undefined) {
            core.failure("无法识别 worktree 基分支");
            return;
          }
          worktreeBase = worktreeBaseFromBranch(branch);
        }
        members.push({
          name: member.name,
          description: member.description,
          machine: member.machine,
          agent: member.agent,
          workspace: session.workspace,
          useWorktree,
          worktreeBase,
        });
      }
      const group = await core.client.createGroup(members, entry.group.project);
      await refreshList(core);
      await openEntry(core, { kind: "group", group });
    }
  } catch (error) {
    core.failure(`新建会话失败：${messageOf(error)}`);
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

/** 拖拽/移动会话到指定项目（None 置为未归属，docs/PRD.md「会话列表视图」）。 */
export async function setEntryProject(
  core: Core,
  entry: ListEntry,
  project: string | undefined,
): Promise<void> {
  if (!core.client) return;
  try {
    if (entry.kind === "session") {
      await core.client.configureSession(
        entry.session.id,
        entry.session.title,
        null,
        project ?? null,
        entry.session.pinned,
      );
      core.update((state) => {
        const item = state.entries.find(
          (candidate) => candidate.kind === "session" && candidate.session.id === entry.session.id,
        );
        if (item?.kind === "session") item.session.project = project;
        if (state.detail.session?.id === entry.session.id) {
          state.detail.session.project = project;
        }
      });
    } else if (entry.kind === "workflow") {
      await core.client.configureWorkflow(
        entry.workflow.id,
        entry.workflow.title,
        project ?? null,
        entry.workflow.pinned,
      );
      core.update((state) => {
        const item = state.entries.find(
          (candidate) =>
            candidate.kind === "workflow" && candidate.workflow.id === entry.workflow.id,
        );
        if (item?.kind === "workflow") item.workflow.project = project;
        if (state.detail.workflow?.id === entry.workflow.id) {
          state.detail.workflow.project = project;
        }
      });
    } else {
      await core.client.configureGroup(
        entry.group.id,
        entry.group.title,
        project ?? null,
        entry.group.pinned,
      );
      core.update((state) => {
        const item = state.entries.find(
          (candidate) => candidate.kind === "group" && candidate.group.id === entry.group.id,
        );
        if (item?.kind === "group") item.group.project = project;
        if (state.detail.group?.id === entry.group.id) {
          state.detail.group.project = project;
        }
      });
    }
    core.success("已更新会话所属项目");
    await refreshList(core);
  } catch (error) {
    core.failure(`设置会话所属项目失败：${messageOf(error)}`);
  }
}

/** 置顶或取消置顶会话（docs/PRD.md「会话列表视图」右键菜单）。 */
export async function toggleEntryPinned(core: Core, entry: ListEntry): Promise<void> {
  if (!core.client) return;
  const pinned = !entryPinned(entry);
  try {
    if (entry.kind === "session") {
      await core.client.configureSession(
        entry.session.id,
        entry.session.title,
        null,
        entry.session.project ?? null,
        pinned,
      );
      core.update((state) => {
        const item = state.entries.find(
          (candidate) =>
            candidate.kind === "session" && candidate.session.id === entry.session.id,
        );
        if (item?.kind === "session") item.session.pinned = pinned;
        if (state.detail.session?.id === entry.session.id) {
          state.detail.session.pinned = pinned;
        }
      });
    } else if (entry.kind === "workflow") {
      await core.client.configureWorkflow(
        entry.workflow.id,
        entry.workflow.title,
        entry.workflow.project ?? null,
        pinned,
      );
      core.update((state) => {
        const item = state.entries.find(
          (candidate) =>
            candidate.kind === "workflow" &&
            candidate.workflow.id === entry.workflow.id,
        );
        if (item?.kind === "workflow") item.workflow.pinned = pinned;
        if (state.detail.workflow?.id === entry.workflow.id) {
          state.detail.workflow.pinned = pinned;
        }
      });
    } else {
      await core.client.configureGroup(
        entry.group.id,
        entry.group.title,
        entry.group.project ?? null,
        pinned,
      );
      core.update((state) => {
        const item = state.entries.find(
          (candidate) => candidate.kind === "group" && candidate.group.id === entry.group.id,
        );
        if (item?.kind === "group") item.group.pinned = pinned;
        if (state.detail.group?.id === entry.group.id) {
          state.detail.group.pinned = pinned;
        }
      });
    }
    await refreshList(core);
  } catch (error) {
    core.failure(`更新置顶状态失败：${messageOf(error)}`);
  }
}

/** 新建项目（docs/PRD.md「项目管理设置」）。 */
export async function createProject(core: Core, name: string, description: string): Promise<void> {
  if (!core.client) return;
  try {
    await core.client.createProject({ name, description });
    core.update((state) => {
      state.settings.projects.push({ name, description });
    });
    core.success("项目已创建");
  } catch (error) {
    core.failure(`创建项目失败：${messageOf(error)}`);
  }
}

/** 更新项目描述（名称不可修改）。 */
export async function updateProject(
  core: Core,
  name: string,
  description: string,
): Promise<void> {
  if (!core.client) return;
  try {
    await core.client.updateProject(name, description);
    core.update((state) => {
      const item = state.settings.projects.find((candidate) => candidate.name === name);
      if (item) item.description = description;
    });
    core.success("项目已更新");
  } catch (error) {
    core.failure(`更新项目失败：${messageOf(error)}`);
  }
}

/** 删除项目：其下会话回到未归属，项目快捷指令一并删除（服务端处理）。 */
export async function deleteProject(core: Core, name: string): Promise<void> {
  if (!core.client) return;
  try {
    await core.client.deleteProject(name);
    core.update((state) => {
      state.settings.projects = state.settings.projects.filter((item) => item.name !== name);
      state.settings.quickCommands = state.settings.quickCommands.filter(
        (item) => item.project !== name,
      );
    });
    core.success("项目已删除");
    await refreshList(core);
  } catch (error) {
    core.failure(`删除项目失败：${messageOf(error)}`);
  }
}

/** 更新项目顺序（docs/PRD.md「项目管理设置」拖拽调整顺序）。 */
export async function setProjectOrder(core: Core, names: string[]): Promise<void> {
  if (!core.client) return;
  const current = core.state.settings.projects;
  try {
    await core.client.setProjectOrder(names);
    const byName = new Map(current.map((item) => [item.name, item]));
    core.update((state) => {
      state.settings.projects = names
        .map((name) => byName.get(name))
        .filter((item): item is Project => item !== undefined);
    });
    core.success("项目顺序已更新");
  } catch (error) {
    core.failure(`更新项目顺序失败：${messageOf(error)}`);
  }
}

/** 普通模式创建会话。 */
export async function createSession(core: Core): Promise<void> {
  if (!core.client) return;
  const { machine, agent, workspace, useWorktree, worktreeBase, project } =
    core.state.newSession;
  let base: WorktreeBase | undefined;
  if (useWorktree) {
    if (worktreeBase === null) {
      core.failure("开启 worktree 时必须指定基分支");
      return;
    }
    base = worktreeBase;
  }
  try {
    const session = await core.client.createSession({
      machine,
      agent,
      workspace,
      useWorktree,
      worktreeBase: base,
      project,
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
  const selectedPlan = core.state.newSession.selectedPlan;
  const project = core.state.newSession.project;
  try {
    const workflow = await core.client.createWorkflow(plan, null, project);
    if (selectedPlan !== null) {
      const plans = core.state.settings.plans.map((item) =>
        item.name === selectedPlan ? { ...item, lastUsedProject: project } : item,
      );
      await core.client.setWorkflowPlans(plans);
      core.update((state) => {
        state.settings.plans = plans;
      });
    }
    core.update((state) => {
      state.newSession = { ...initialNewSession(), mode: state.newSession.mode };
    });
    await refreshList(core);
    await openEntry(core, { kind: "workflow", workflow });
  } catch (error) {
    core.failure(`创建工作流会话失败：${messageOf(error)}`);
  }
}

/** 群聊模式创建会话。 */
export async function createGroup(core: Core): Promise<void> {
  if (!core.client) return;
  const { groupMembers, project } = core.state.newSession;
  try {
    const group = await core.client.createGroup(groupMembers, project);
    core.update((state) => {
      state.newSession = { ...initialNewSession(), mode: state.newSession.mode };
    });
    await refreshList(core);
    await openEntry(core, { kind: "group", group });
  } catch (error) {
    core.failure(`创建群聊会话失败：${messageOf(error)}`);
  }
}

// ---------- 会话交互 ----------

function nextAttachmentId(): string {
  return globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random()}`;
}

function updatePendingAttachment(
  core: Core,
  id: string,
  update: (attachment: PendingAttachment) => void,
): void {
  core.update((state) => {
    const attachment = state.attachments.find((item) => item.id === id);
    if (attachment !== undefined) update(attachment);
  });
}

async function uploadPendingAttachment(core: Core, id: string): Promise<void> {
  const attachment = core.state.attachments.find((item) => item.id === id);
  const target = core.state.open;
  const client = core.client;
  if (attachment === undefined || target === null || client === null) return;
  const controller = new AbortController();
  updatePendingAttachment(core, id, (item) => {
    item.status = "uploading";
    item.error = undefined;
    item.controller = controller;
  });
  try {
    const uploaded =
      target.kind === "session"
        ? await client.uploadSessionAttachment(target.id, attachment.file, controller.signal)
        : target.kind === "workflow"
          ? await client.uploadWorkflowAttachment(target.id, attachment.file, controller.signal)
          : await client.uploadGroupAttachment(target.id, attachment.file, controller.signal);
    if (core.state.open?.kind !== target.kind || core.state.open.id !== target.id) {
      await deleteRemoteAttachment(client, target, uploaded.name);
      core.update((state) => {
        state.attachments = state.attachments.filter((item) => item.id !== id);
      });
      return;
    }
    if (!core.state.attachments.some((item) => item.id === id)) {
      await deleteRemoteAttachment(client, target, uploaded.name);
      return;
    }
    updatePendingAttachment(core, id, (item) => {
      item.status = "uploaded";
      item.controller = undefined;
      item.remoteName = uploaded.name;
      item.block = {
        type: "resource_link",
        uri:
          target.kind === "session"
            ? client.sessionAttachmentUri(target.id, uploaded.name)
            : target.kind === "workflow"
              ? client.workflowAttachmentUri(target.id, uploaded.name)
              : client.groupAttachmentUri(target.id, uploaded.name),
        name: item.label,
        mimeType: item.file.type || undefined,
      };
    });
    if (core.state.sidePanel === "attachments") {
      void refreshAttachments(core);
    }
  } catch (error) {
    if (controller.signal.aborted) return;
    updatePendingAttachment(core, id, (item) => {
      item.status = "failed";
      item.controller = undefined;
      item.error = messageOf(error);
    });
  }
}

/** 附件上传到 Server 后，以公共 URI 的 Resource Link 加入提示词。 */
export async function addFiles(core: Core, files: FileList | File[]): Promise<void> {
  const target = core.state.open;
  if (target === null || core.client === null) return;
  const ids: string[] = [];
  for (const file of files) {
    if (file.size > 20 * 1024 * 1024) {
      core.failure(`附件「${file.name}」超过 20 MB`);
      continue;
    }
    const id = nextAttachmentId();
    core.update((state) => {
      state.attachments = [
        ...state.attachments,
        {
          id,
          label: file.name,
          file,
          status: "uploading",
        },
      ];
    });
    ids.push(id);
  }
  await Promise.all(ids.map((id) => uploadPendingAttachment(core, id)));
}

export async function retryAttachment(core: Core, id: string): Promise<void> {
  await uploadPendingAttachment(core, id);
}

export async function removeAttachment(core: Core, id: string): Promise<void> {
  const attachment = core.state.attachments.find((item) => item.id === id);
  const target = core.state.open;
  if (attachment === undefined || target === null || core.client === null) return;
  attachment.controller?.abort();
  core.update((state) => {
    state.attachments = state.attachments.filter((item) => item.id !== id);
  });
  if (attachment.status !== "uploaded" || attachment.remoteName === undefined) return;
  try {
    await deleteRemoteAttachment(core.client, target, attachment.remoteName);
    if (core.state.sidePanel === "attachments") {
      await refreshAttachments(core);
    }
  } catch (error) {
    core.failure(`删除附件失败：${messageOf(error)}`);
  }
}

const ATTACHMENTS_PAGE_SIZE = 50;

async function deleteRemoteAttachment(
  client: ApiClient,
  target: OpenTarget,
  name: string,
): Promise<void> {
  if (target.kind === "session") {
    await client.deleteSessionAttachment(target.id, name);
  } else if (target.kind === "workflow") {
    await client.deleteWorkflowAttachment(target.id, name);
  } else {
    await client.deleteGroupAttachment(target.id, name);
  }
}

async function loadAttachmentPage(
  client: ApiClient,
  target: OpenTarget,
  limit: number,
  offset: number,
) {
  if (target.kind === "session") return client.sessionAttachments(target.id, limit, offset);
  if (target.kind === "workflow") return client.workflowAttachments(target.id, limit, offset);
  return client.groupAttachments(target.id, limit, offset);
}

/** 会话附件面板打开时刷新第一页。 */
export async function refreshAttachments(core: Core): Promise<void> {
  const target = core.state.open;
  const client = core.client;
  if (target === null || client === null) return;
  core.update((state) => {
    state.detail.attachmentsLoading = true;
  });
  try {
    const page = await loadAttachmentPage(client, target, ATTACHMENTS_PAGE_SIZE, 0);
    core.update((state) => {
      if (state.open?.kind !== target.kind || state.open.id !== target.id) return;
      state.detail.attachments = page.attachments;
      state.detail.attachmentsHasMore = page.hasMore;
      state.detail.attachmentsLoading = false;
    });
  } catch (error) {
    core.update((state) => {
      state.detail.attachmentsLoading = false;
    });
    core.failure(`读取附件失败：${messageOf(error)}`);
  }
}

/** 加载下一页附件。 */
export async function loadMoreAttachments(core: Core): Promise<void> {
  const target = core.state.open;
  const client = core.client;
  if (
    target === null ||
    client === null ||
    core.state.detail.attachmentsLoading ||
    !core.state.detail.attachmentsHasMore
  ) {
    return;
  }
  const offset = core.state.detail.attachments.length;
  core.update((state) => {
    state.detail.attachmentsLoading = true;
  });
  try {
    const page = await loadAttachmentPage(client, target, ATTACHMENTS_PAGE_SIZE, offset);
    core.update((state) => {
      if (state.open?.kind !== target.kind || state.open.id !== target.id) return;
      state.detail.attachments = [...state.detail.attachments, ...page.attachments];
      state.detail.attachmentsHasMore = page.hasMore;
      state.detail.attachmentsLoading = false;
    });
  } catch (error) {
    core.update((state) => {
      state.detail.attachmentsLoading = false;
    });
    core.failure(`加载更多附件失败：${messageOf(error)}`);
  }
}

/** 删除一个已保存附件。 */
export async function deleteAttachment(core: Core, name: string): Promise<void> {
  const target = core.state.open;
  const client = core.client;
  if (target === null || client === null) return;
  try {
    await deleteRemoteAttachment(client, target, name);
    core.update((state) => {
      state.attachments = state.attachments.filter(
        (attachment) => attachment.remoteName !== name,
      );
    });
    await refreshAttachments(core);
  } catch (error) {
    core.failure(`删除附件失败：${messageOf(error)}`);
  }
}

/** 删除当前会话全部附件。 */
export async function deleteAllAttachments(core: Core): Promise<void> {
  const target = core.state.open;
  const client = core.client;
  if (target === null || client === null) return;
  try {
    if (target.kind === "session") {
      await client.deleteSessionAttachments(target.id);
    } else if (target.kind === "workflow") {
      await client.deleteWorkflowAttachments(target.id);
    } else {
      await client.deleteGroupAttachments(target.id);
    }
    core.update((state) => {
      state.detail.attachments = [];
      state.detail.attachmentsHasMore = false;
      for (const attachment of state.attachments) attachment.controller?.abort();
      state.attachments = [];
    });
  } catch (error) {
    core.failure(`删除全部附件失败：${messageOf(error)}`);
  }
}

/** 仅输入框发送会消费待发送附件。 */
export async function sendPrompt(
  core: Core,
  text: string,
  includeAttachments = true,
): Promise<boolean> {
  const target = core.state.open;
  if (!core.client || !target) return false;
  if (
    includeAttachments &&
    core.state.attachments.some((attachment) => attachment.status !== "uploaded")
  ) {
    core.failure("请等待附件上传完成或处理上传失败项");
    return false;
  }
  const input: ContentBlock[] = [];
  if (text !== "") input.push({ type: "text", text });
  if (includeAttachments) {
    input.push(
      ...core.state.attachments.flatMap((attachment) =>
        attachment.block === undefined ? [] : [attachment.block],
      ),
    );
  }
  if (input.length === 0) return false;
  try {
    if (target.kind === "session") {
      await core.client.promptSession(target.id, input);
    } else if (target.kind === "workflow") {
      await core.client.promptWorkflow(target.id, input);
    } else {
      await core.client.promptGroup(target.id, input);
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
    } else if (target.kind === "workflow") {
      await core.client.promptWorkflow(target.id, [
        { type: "text", text: "取消当前进行中的全部工作" },
      ]);
      await refreshList(core);
      core.last.list = Date.now();
    } else {
      await core.client.cancelGroup(target.id);
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
  const session = core.state.detail.session;
  if (session === null) return;
  try {
    await core.client.configureSession(
      target.id,
      session.title,
      { configId, ...value },
      session.project ?? null,
      session.pinned,
    );
    const options = await core.client.configOptions(target.id);
    core.update((state) => {
      state.detail.configOptions = options;
    });
  } catch (error) {
    core.failure(`设置会话选项失败：${messageOf(error)}`);
  }
}

// ---------- 终端 ----------

/** 打开终端：以会话执行目录为 cwd，返回的终端立即成为活动终端。 */
export async function openTerminal(core: Core, cols: number, rows: number): Promise<void> {
  const target = core.state.open;
  if (!core.client || !target || target.kind !== "session") return;
  const session = core.state.detail.session;
  try {
    const terminalId = await core.client.openTerminal(
      target.id,
      session ? execDir(session) : null,
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
  if (config.baseUrl.trim() === "" || config.apiKey.trim() === "" || config.model.trim() === "") {
    core.failure("保存失败：请填写 Base URL、API Key 与模型名称");
    return;
  }
  if (!Number.isSafeInteger(config.contextWindow) || config.contextWindow <= 0) {
    core.failure("保存失败：上下文窗口必须是正整数");
    return;
  }
  try {
    await core.client.setOrchestrator(config);
    core.update((state) => {
      state.settings.orchestrator = { status: "ready", config };
    });
    core.success("工作流智能体配置已保存");
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

export async function saveGroupMembers(
  core: Core,
  members: GroupMemberTemplate[],
): Promise<void> {
  if (!core.client) return;
  try {
    await core.client.setGroupMembers(members);
    core.update((state) => {
      state.settings.groupMembers = members;
    });
    core.success("群聊预设成员已保存");
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
