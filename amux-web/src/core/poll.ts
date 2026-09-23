// 各视图的刷新机制（docs/DESIGN.md「应用」各节）。
//
// 单任务节拍内按到期时间触发各视图刷新；设置类数据不做定时刷新，由视图打开时实时拉取。

import type { Activity, HistoryItem, ListEntry, OpenTarget } from "../lib/types";
import { entryId } from "../lib/types";
import {
  beginNewerPage,
  beginOlderPage,
  prependNewer,
  prependOlder,
  refreshFetch,
  refreshLimit,
  replaceWindow,
  trimNewest,
  trimOldest,
} from "../lib/paging";
import { buildListWindow, sameListEntries, sortEntries } from "../lib/list";
import { decodeBase64 } from "../lib/terminal";
import { ApiError } from "../lib/api";
import type { Core, SettingsTab } from "./core";

export const SESSION_LIST_INTERVAL = 10_000;
export const HISTORY_INTERVAL = 5_000;
export const ONGOING_INTERVAL = 2_000;
export const ACTIVITIES_INTERVAL = 10_000;
export const PLAN_INTERVAL = 10_000;
export const RECONNECT_INTERVAL = 5_000;
const TERMINAL_RETRY_DELAY = 500;

/** 节拍粒度：各视图按其周期在节拍内到期触发。 */
export const TICK_INTERVAL = 250;

export function due(last: number | undefined, interval: number, now = Date.now()): boolean {
  return last === undefined || now - last >= interval;
}

/** 单次节拍：按需刷新各视图。 */
export async function tick(core: Core): Promise<void> {
  const client = core.client;
  if (!client) {
    stopTerminalStream(core);
    return;
  }

  // 连接检查：断开时周期重连，成功后重置逐视图节拍与打开视图的已加载标记
  if (core.state.status !== "online") {
    stopTerminalStream(core);
    if (due(core.last.reconnect, RECONNECT_INTERVAL)) {
      core.last.reconnect = Date.now();
      try {
        await client.ping();
        core.update((state) => {
          state.status = "online";
          state.error = null;
        });
        core.last = {};
      } catch (error) {
        core.update((state) => {
          state.status = "failed";
          state.error = messageOf(error);
        });
      }
    }
    return;
  }

  if (due(core.last.list, SESSION_LIST_INTERVAL)) {
    core.last.list = Date.now();
    await refreshList(core);
  }

  syncTerminalStream(core);
  if (core.state.open && core.state.middle === "interaction") {
    await refreshOpen(core);
  }
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

// ---------- 会话列表 ----------

/**
 * 会话列表刷新：重取已加载窗口（普通会话与工作流会话各取一窗）。
 *
 * 窗口贴着最新一端，删改与排序变化都在整窗重取后自然生效。
 */
export async function refreshList(core: Core): Promise<void> {
  const client = core.client;
  if (!client) return;
  const limit = refreshLimit(core.state.entries.length, core.state.listPaging.pageSize);
  try {
    const [sessions, workflows] = await Promise.all([
      client.sessions(limit, 0),
      client.workflows(limit, 0),
    ]);
    core.update((state) => {
      const window = buildListWindow(
        state.listPaging,
        sessions.sessions,
        workflows.workflows,
        sessions.hasMore || workflows.hasMore,
      );
      if (!sameListEntries(state.entries, window.entries)) {
        state.entries = window.entries;
      }
      state.listPaging = window.paging;
      state.listRefreshVersion += 1;
    });
  } catch (error) {
    core.failure(`刷新会话列表失败：${messageOf(error)}`);
  }
}

/** 会话列表更早一页：两个来源各取一页并合并到窗口。 */
export async function loadOlderList(core: Core): Promise<void> {
  const client = core.client;
  if (!client) return;
  const started = beginOlderPage(core.state.listPaging, core.state.entries.length);
  if (!started) return;
  core.update((state) => {
    state.listPaging = started.paging;
  });
  // 普通会话与工作流会话是独立分页的两个列表，偏移各自按已加载条数计算；
  // 用合并后总条数当 offset 会跳过两端的中间页（issue：普通会话和工作流混合翻页会跳数）。
  const sessionsOffset = core.state.entries.filter((entry) => entry.kind === "session").length;
  const workflowsOffset = core.state.entries.filter((entry) => entry.kind === "workflow").length;
  try {
    const [sessions, workflows] = await Promise.all([
      client.sessions(started.limit, sessionsOffset),
      client.workflows(started.limit, workflowsOffset),
    ]);
    core.update((state) => {
      const page: ListEntry[] = [
        ...sessions.sessions.map((session): ListEntry => ({ kind: "session", session })),
        ...workflows.workflows.map((workflow): ListEntry => ({ kind: "workflow", workflow })),
      ];
      const pageIds = new Set(page.map(entryId));
      const kept = state.entries.filter((entry) => !pageIds.has(entryId(entry)));
      state.entries = sortEntries([...kept, ...page]);
      state.listPaging = {
        ...state.listPaging,
        loadingOlder: false,
        hasOlder: sessions.hasMore || workflows.hasMore,
      };
    });
  } catch (error) {
    core.update((state) => {
      state.listPaging = { ...state.listPaging, loadingOlder: false };
    });
    core.failure(`加载更早会话失败：${messageOf(error)}`);
  }
}

// ---------- 打开会话的视图数据 ----------

async function refreshOpen(core: Core): Promise<void> {
  const target = core.state.open;
  if (!target) return;
  const activitiesOpen = core.state.sidePanel === "activities";
  const planOpen = core.state.sidePanel === "plan";

  // 会话详情与上下文用量只在详情视图打开时刷新一次（refreshDetails），不随节拍拉取
  if (due(core.last.history, HISTORY_INTERVAL)) {
    core.last.history = Date.now();
    await refreshHistory(core);
  }
  if (activitiesOpen && due(core.last.activities, ACTIVITIES_INTERVAL)) {
    core.last.activities = Date.now();
    await refreshActivities(core);
  }
  if (planOpen && target.kind === "session" && due(core.last.plan, PLAN_INTERVAL)) {
    core.last.plan = Date.now();
    try {
      const entries = await core.client!.plan(target.id);
      core.update((state) => {
        if (sameTarget(state.open, target)) state.detail.plan = entries;
      });
    } catch {
      // 计划仅普通会话有；失败留待下一周期
    }
  }
  if (due(core.last.ongoing, ONGOING_INTERVAL)) {
    core.last.ongoing = Date.now();
    try {
      const activity =
        target.kind === "session"
          ? await core.client!.ongoingActivity(target.id)
          : await core.client!.workflowOngoingActivity(target.id);
      core.update((state) => {
        if (sameTarget(state.open, target)) state.detail.ongoing = activity;
      });
    } catch {
      // 忽略：下一周期重试
    }
  }
}

function sameTarget(a: OpenTarget | null, b: OpenTarget): boolean {
  return a !== null && a.kind === b.kind && a.id === b.id;
}

type TerminalStreamSession = { key: string; controller: AbortController };
const terminalStreams = new WeakMap<Core, TerminalStreamSession>();

/** 停止终端流；面板收起、终端切换和 Core 销毁时调用。 */
export function stopTerminalStream(core: Core): void {
  const session = terminalStreams.get(core);
  if (session) session.controller.abort();
  terminalStreams.delete(core);
}

/** 重建终端流并获取完整快照；xterm 实例重建后调用。 */
export function restartTerminalStream(core: Core): void {
  stopTerminalStream(core);
  syncTerminalStream(core);
}

/** 同步 SSE 生命周期：只在普通会话的终端面板打开且已有活动终端时连接。 */
export function syncTerminalStream(core: Core): void {
  const target = core.state.open;
  const terminal = core.state.detail.activeTerminal;
  if (
    core.client === null ||
    core.state.status !== "online" ||
    core.state.middle !== "interaction" ||
    core.state.sidePanel !== "terminal" ||
    target?.kind !== "session" ||
    terminal === null
  ) {
    stopTerminalStream(core);
    return;
  }

  const key = `${target.id}\0${terminal}`;
  const current = terminalStreams.get(core);
  if (current?.key === key) return;
  if (current) current.controller.abort();

  const session = { key, controller: new AbortController() };
  terminalStreams.set(core, session);
  void consumeTerminalStream(core, key, session.controller);
}

async function consumeTerminalStream(
  core: Core,
  key: string,
  controller: AbortController,
): Promise<void> {
  while (isCurrentTerminalStream(core, key, controller)) {
    let first = true;
    let lastCursor: number | undefined;
    const client = core.client;
    const target = core.state.open;
    const terminal = core.state.detail.activeTerminal;
    if (client === null || target?.kind !== "session" || terminal === null) break;
    try {
      await client.terminalOutputStream(
        target.id,
        terminal,
        controller.signal,
        (output) => {
          if (!isCurrentTerminalStream(core, key, controller)) return;
          if (lastCursor !== undefined && output.nextCursor <= lastCursor) return;
          const reset = first || output.truncated === true;
          first = false;
          lastCursor = output.nextCursor;
          const bytes = decodeBase64(output.data);
          if (bytes.length === 0 && !reset) return;
          core.pushTerminalChunk(bytes, reset);
        },
      );
    } catch (error) {
      if (controller.signal.aborted || core.client === null) break;
      if (error instanceof ApiError && error.status === 404) {
        core.update((state) => {
          if (state.detail.activeTerminal === terminal) {
            state.detail.activeTerminal = null;
            state.detail.terminalSeq += 1;
            state.detail.terminalChunks.push({
              seq: state.detail.terminalSeq,
              bytes: new Uint8Array(0),
              reset: true,
            });
          }
        });
        break;
      }
      // 连接失败时保留当前画面，短暂退避后重新建立 SSE。
      console.warn("终端流中断，准备重连", error);
    }
    if (controller.signal.aborted) break;
    await abortableDelay(TERMINAL_RETRY_DELAY, controller.signal);
  }
  if (terminalStreams.get(core)?.controller === controller) {
    terminalStreams.delete(core);
  }
}

function isCurrentTerminalStream(
  core: Core,
  key: string,
  controller: AbortController,
): boolean {
  const target = core.state.open;
  const terminal = core.state.detail.activeTerminal;
  return (
    !controller.signal.aborted &&
    core.state.middle === "interaction" &&
    core.state.sidePanel === "terminal" &&
    target?.kind === "session" &&
    terminal !== null &&
    `${target.id}\0${terminal}` === key &&
    terminalStreams.get(core)?.key === key &&
    terminalStreams.get(core)?.controller === controller
  );
}

function abortableDelay(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    signal.addEventListener(
      "abort",
      () => {
        clearTimeout(timer);
        resolve();
      },
      { once: true },
    );
  });
}

/** 对话历史刷新：按当前窗口取数并整窗替换，拉取量不超过服务端单次上限。 */
export async function refreshHistory(core: Core): Promise<void> {
  const target = core.state.open;
  if (!target) return;
  const paging = core.state.detail.historyPaging;
  const loaded = core.state.detail.history.length;
  const { offset, limit } = refreshFetch(paging, loaded);
  try {
    const page =
      target.kind === "session"
        ? await core.client!.history(target.id, limit, offset)
        : await core.client!.workflowHistory(target.id, limit, offset);
    core.update((state) => {
      if (!sameTarget(state.open, target)) return;
      const merged = replaceWindow<HistoryItem>(
        state.detail.history,
        state.detail.historyPaging,
        page.items,
        offset,
        page.hasMore,
      );
      state.detail.history = merged.items;
      state.detail.historyPaging = merged.paging;
    });
  } catch (error) {
    core.failure(`读取对话失败：${messageOf(error)}`);
  }
}

/** 对话历史更早一页：插到窗口前面，并锚定滚动位置。 */
export async function loadOlderHistory(core: Core): Promise<void> {
  const target = core.state.open;
  if (!target) return;
  const started = beginOlderPage(
    core.state.detail.historyPaging,
    core.state.detail.history.length,
  );
  if (!started) return;
  core.update((state) => {
    state.detail.historyPaging = started.paging;
  });
  try {
    const page =
      target.kind === "session"
        ? await core.client!.history(target.id, started.limit, started.offset)
        : await core.client!.workflowHistory(target.id, started.limit, started.offset);
    core.update((state) => {
      if (!sameTarget(state.open, target)) return;
      const merged = prependOlder<HistoryItem>(
        state.detail.history,
        state.detail.historyPaging,
        page.items,
      );
      const trimmed = trimNewest(merged.items, merged.paging);
      state.detail.history = trimmed.items;
      state.detail.historyPaging = { ...trimmed.paging, hasOlder: page.hasMore };
    });
  } catch (error) {
    core.update((state) => {
      state.detail.historyPaging = { ...state.detail.historyPaging, loadingOlder: false };
    });
    core.failure(`加载更早对话失败：${messageOf(error)}`);
  }
}

/** 对话历史更新一页：插到窗口末尾（底部），供用户回到较新内容时加载。 */
export async function loadNewerHistory(core: Core): Promise<void> {
  const target = core.state.open;
  if (!target) return;
  const started = beginNewerPage(core.state.detail.historyPaging);
  if (!started) return;
  core.update((state) => {
    state.detail.historyPaging = started.paging;
  });
  try {
    const page =
      target.kind === "session"
        ? await core.client!.history(target.id, started.limit, started.offset)
        : await core.client!.workflowHistory(target.id, started.limit, started.offset);
    core.update((state) => {
      if (!sameTarget(state.open, target)) return;
      const merged = prependNewer<HistoryItem>(
        state.detail.history,
        state.detail.historyPaging,
        page.items,
        started.offset,
      );
      const trimmed = trimOldest(merged.items, merged.paging);
      state.detail.history = trimmed.items;
      state.detail.historyPaging = trimmed.paging;
    });
  } catch (error) {
    core.update((state) => {
      state.detail.historyPaging = { ...state.detail.historyPaging, loadingNewer: false };
    });
    core.failure(`加载更新对话失败：${messageOf(error)}`);
  }
}

/** 活动历史刷新：按当前窗口取数并整窗替换，拉取量不超过服务端单次上限。 */
export async function refreshActivities(core: Core): Promise<void> {
  const target = core.state.open;
  if (!target) return;
  const paging = core.state.detail.activitiesPaging;
  const loaded = core.state.detail.activities.length;
  const { offset, limit } = refreshFetch(paging, loaded);
  try {
    const page =
      target.kind === "session"
        ? await core.client!.activities(target.id, limit, offset)
        : await core.client!.workflowActivities(target.id, limit, offset);
    core.update((state) => {
      if (!sameTarget(state.open, target)) return;
      const merged = replaceWindow<Activity>(
        state.detail.activities,
        state.detail.activitiesPaging,
        page.activities,
        offset,
        page.hasMore,
      );
      state.detail.activities = merged.items;
      state.detail.activitiesPaging = merged.paging;
    });
  } catch (error) {
    core.failure(`读取活动失败：${messageOf(error)}`);
  }
}

/** 活动历史更早一页。 */
export async function loadOlderActivities(core: Core): Promise<void> {
  const target = core.state.open;
  if (!target) return;
  const started = beginOlderPage(
    core.state.detail.activitiesPaging,
    core.state.detail.activities.length,
  );
  if (!started) return;
  core.update((state) => {
    state.detail.activitiesPaging = started.paging;
  });
  try {
    const page =
      target.kind === "session"
        ? await core.client!.activities(target.id, started.limit, started.offset)
        : await core.client!.workflowActivities(target.id, started.limit, started.offset);
    core.update((state) => {
      if (!sameTarget(state.open, target)) return;
      const merged = prependOlder<Activity>(
        state.detail.activities,
        state.detail.activitiesPaging,
        page.activities,
      );
      const trimmed = trimNewest(merged.items, merged.paging);
      state.detail.activities = trimmed.items;
      state.detail.activitiesPaging = { ...trimmed.paging, hasOlder: page.hasMore };
    });
  } catch (error) {
    core.update((state) => {
      state.detail.activitiesPaging = { ...state.detail.activitiesPaging, loadingOlder: false };
    });
    core.failure(`加载更早活动失败：${messageOf(error)}`);
  }
}

/** 活动历史更新一页：插到窗口末尾（底部）。 */
export async function loadNewerActivities(core: Core): Promise<void> {
  const target = core.state.open;
  if (!target) return;
  const started = beginNewerPage(core.state.detail.activitiesPaging);
  if (!started) return;
  core.update((state) => {
    state.detail.activitiesPaging = started.paging;
  });
  try {
    const page =
      target.kind === "session"
        ? await core.client!.activities(target.id, started.limit, started.offset)
        : await core.client!.workflowActivities(target.id, started.limit, started.offset);
    core.update((state) => {
      if (!sameTarget(state.open, target)) return;
      const merged = prependNewer<Activity>(
        state.detail.activities,
        state.detail.activitiesPaging,
        page.activities,
        started.offset,
      );
      const trimmed = trimOldest(merged.items, merged.paging);
      state.detail.activities = trimmed.items;
      state.detail.activitiesPaging = trimmed.paging;
    });
  } catch (error) {
    core.update((state) => {
      state.detail.activitiesPaging = { ...state.detail.activitiesPaging, loadingNewer: false };
    });
    core.failure(`加载更新活动失败：${messageOf(error)}`);
  }
}

/** 终端视图打开时从 Server 拉取一次终端列表，并移除服务端已消失的终端；不做周期性轮询。 */
export async function refreshTerminalList(core: Core): Promise<void> {
  const client = core.client;
  const target = core.state.open;
  if (!client || !target || target.kind !== "session" || core.state.sidePanel !== "terminal") {
    return;
  }
  try {
    const terminals = await client.terminals(target.id);
    core.update((state) => {
      if (state.sidePanel !== "terminal" || !sameTarget(state.open, target)) return;
      const active = state.detail.activeTerminal;
      state.detail.terminals = terminals;
      if (active === null || !terminals.some((terminal) => terminal.id === active)) {
        state.detail.activeTerminal = terminals[0]?.id ?? null;
        state.detail.terminalSeq += 1;
        state.detail.terminalChunks.push({
          seq: state.detail.terminalSeq,
          bytes: new Uint8Array(0),
          reset: true,
        });
      }
    });
    syncTerminalStream(core);
  } catch {
    // 读取失败仅影响当前打开这次；下次视图打开时再重试
  }
}

// ---------- 视图打开时的实时拉取 ----------

/** 新建视图打开时刷新，不依赖当前表单模式。 */
export async function refreshNewSession(core: Core): Promise<void> {
  await Promise.all([
    refreshMachines(core),
    refreshRecentWorkspaces(core),
    refreshPlans(core),
    refreshProjects(core),
  ]);
}

/** 进入工作流模式时检查内置智能体是否已配置。 */
export async function refreshWorkflowSetup(core: Core): Promise<void> {
  await refreshOrchestrator(core);
}

/** 会话交互视图常驻数据：机器/agents（可用性标记）、内置智能体配置与快捷指令。 */
export async function refreshInteraction(core: Core): Promise<void> {
  await Promise.all([
    refreshMachines(core),
    refreshOrchestrator(core),
    refreshQuickCommands(core),
    refreshSessionControls(core),
    refreshTerminalList(core),
  ]);
}

async function refreshSessionControls(core: Core): Promise<void> {
  const client = core.client;
  const target = core.state.open;
  if (!client || target?.kind !== "session") return;
  // 查询选项会惰性创建或恢复 ACP 会话，随后再读取其已发布的斜杠命令。
  try {
    const options = await client.configOptions(target.id);
    core.update((state) => {
      if (sameTarget(state.open, target)) state.detail.configOptions = options;
    });
  } catch (error) {
    core.failure(`读取会话选项失败：${messageOf(error)}`);
  }
  if (!sameTarget(core.state.open, target)) return;
  try {
    const commands = await client.slashCommands(target.id);
    core.update((state) => {
      if (sameTarget(state.open, target)) state.detail.slashCommands = commands;
    });
  } catch (error) {
    core.failure(`读取斜杠命令失败：${messageOf(error)}`);
  }
}

/** 设置浮窗当前分类的配置数据（均实时获取，不做定时刷新）。 */
export async function refreshSettings(core: Core, tab: SettingsTab): Promise<void> {
  switch (tab) {
    case "connection":
      return;
    case "machines":
      return refreshMachines(core);
    case "orchestrator":
      return refreshOrchestrator(core);
    case "quickCommands":
      await Promise.all([refreshQuickCommands(core), refreshProjects(core)]);
      return;
    case "skills":
      return refreshSkills(core);
    case "plans":
      return refreshPlans(core);
    case "projects":
      return refreshProjects(core);
  }
}

export async function refreshProjects(core: Core): Promise<void> {
  const client = core.client;
  if (!client) return;
  try {
    const projects = await client.projects();
    core.update((state) => {
      state.settings.projects = projects;
    });
  } catch (error) {
    core.failure(`读取项目失败：${messageOf(error)}`);
  }
}

/** 机器与各机器上的 agents。 */
export async function refreshMachines(core: Core): Promise<void> {
  const client = core.client;
  if (!client) return;
  try {
    const machines = await client.machines();
    const agents = await Promise.all(
      machines.map(async (machine) => ({
        machine: machine.name,
        agents: await client.agents(machine.name).catch(() => []),
      })),
    );
    core.update((state) => {
      state.settings.machines = machines;
      state.settings.agents = agents;
    });
  } catch (error) {
    core.failure(`读取机器列表失败：${messageOf(error)}`);
  }
}

export async function refreshRecentWorkspaces(core: Core): Promise<void> {
  const client = core.client;
  if (!client) return;
  try {
    const recent = await client.recentWorkspaces();
    core.update((state) => {
      state.recentWorkspaces = recent;
    });
  } catch {
    // 最近工作目录仅为便利信息，失败不打扰用户
  }
}

export async function refreshOrchestrator(core: Core): Promise<void> {
  const client = core.client;
  if (!client) return;
  try {
    const config = await client.orchestrator();
    core.update((state) => {
      state.settings.orchestrator = { status: "ready", config };
    });
  } catch (error) {
    // 读取失败不能当作「未配置」：视图需据此拒绝创建工作流会话并给出重试入口
    core.update((state) => {
      state.settings.orchestrator = { status: "failed", error: messageOf(error) };
    });
  }
}

export async function refreshPlans(core: Core): Promise<void> {
  const client = core.client;
  if (!client) return;
  try {
    const plans = await client.workflowPlans();
    core.update((state) => {
      state.settings.plans = plans;
    });
  } catch (error) {
    core.failure(`读取工作流计划失败：${messageOf(error)}`);
  }
}

export async function refreshQuickCommands(core: Core): Promise<void> {
  const client = core.client;
  if (!client) return;
  try {
    const commands = await client.quickCommands();
    core.update((state) => {
      state.settings.quickCommands = commands;
    });
  } catch (error) {
    core.failure(`读取快捷指令失败：${messageOf(error)}`);
  }
}

export async function refreshSkills(core: Core): Promise<void> {
  const client = core.client;
  if (!client) return;
  try {
    const skills = await client.skills();
    core.update((state) => {
      state.settings.skills = skills;
    });
  } catch (error) {
    core.failure(`读取技能失败：${messageOf(error)}`);
  }
}

/** 会话详情视图：打开时刷新一次，不定时刷新（docs/DESIGN.md「会话详情视图」）。 */
export async function refreshDetails(core: Core): Promise<void> {
  const client = core.client;
  const target = core.state.open;
  if (!client || !target) return;
  if (target.kind === "session") {
    try {
      const session = await client.session(target.id);
      core.update((state) => {
        if (!sameTarget(state.open, target)) return;
        state.detail.session = session;
      });
    } catch (error) {
      core.failure(`读取会话详情失败：${messageOf(error)}`);
    }
    try {
      const context = await client.context(target.id);
      core.update((state) => {
        if (!sameTarget(state.open, target)) return;
        state.detail.contextSize = context.contextSize;
        state.detail.contextWindowSize = context.contextWindowSize;
      });
    } catch {
      // 上下文用量需 agent 侧就绪；失败时该行留空
    }
    return;
  }
  try {
    const workflow = await client.workflow(target.id);
    core.update((state) => {
      if (!sameTarget(state.open, target)) return;
      state.detail.workflow = workflow;
    });
  } catch (error) {
    core.failure(`读取会话详情失败：${messageOf(error)}`);
  }
}
