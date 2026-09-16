// 各视图的刷新机制（docs/DESIGN.md「应用」各节）。
//
// 单任务节拍内按到期时间触发各视图刷新；设置类数据不做定时刷新，由视图打开时实时拉取。

import type { Activity, HistoryItem, ListEntry, OpenTarget } from "../lib/types";
import { entryId, entryUpdatedAt } from "../lib/types";
import { beginOlderPage, mergeNewest, prependOlder, refreshLimit } from "../lib/paging";
import { mergeListPage } from "../lib/list";
import { appendTerminalOutput } from "../lib/terminal";
import type { Core, SettingsTab } from "./core";

export const SESSION_LIST_INTERVAL = 10_000;
export const HISTORY_INTERVAL = 5_000;
export const ONGOING_INTERVAL = 2_000;
export const ACTIVITIES_INTERVAL = 10_000;
export const PLAN_INTERVAL = 10_000;
/** 会话选项与斜杠命令由 agent 侧异步推送，取与对话视图相同的周期。 */
export const OPTIONS_INTERVAL = 5_000;
export const TERMINAL_INTERVAL = 500;
export const RECONNECT_INTERVAL = 5_000;

/** 节拍粒度：各视图按其周期在节拍内到期触发。 */
export const TICK_INTERVAL = 250;

export function due(last: number | undefined, interval: number, now = Date.now()): boolean {
  return last === undefined || now - last >= interval;
}

/** 单次节拍：按需刷新各视图。 */
export async function tick(core: Core): Promise<void> {
  const client = core.client;
  if (!client) return;

  // 连接检查：断开时周期重连，成功后重置逐视图节拍与打开视图的已加载标记
  if (core.state.status !== "online") {
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
  const limit = Math.max(core.state.listPaging.pageSize, core.state.entries.length);
  try {
    const [sessions, workflows] = await Promise.all([
      client.sessions(limit, 0),
      client.workflows(limit, 0),
    ]);
    core.update((state) => {
      const merged = mergeListPage(
        state.entries,
        state.listPaging,
        sessions.sessions,
        workflows.workflows,
        sessions.hasMore || workflows.hasMore,
      );
      state.entries = merged.entries;
      state.listPaging = merged.paging;
    });
  } catch (error) {
    core.update((state) => {
      state.status = "failed";
      state.error = messageOf(error);
    });
    core.last.reconnect = Date.now();
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
  try {
    const [sessions, workflows] = await Promise.all([
      client.sessions(started.limit, started.offset),
      client.workflows(started.limit, started.offset),
    ]);
    core.update((state) => {
      const page: ListEntry[] = [
        ...sessions.sessions.map((session): ListEntry => ({ kind: "session", session })),
        ...workflows.workflows.map((workflow): ListEntry => ({ kind: "workflow", workflow })),
      ];
      const pageIds = new Set(page.map(entryId));
      const kept = state.entries.filter((entry) => !pageIds.has(entryId(entry)));
      const merged = [...kept, ...page].sort((a, b) => entryUpdatedAt(b) - entryUpdatedAt(a));
      state.entries = merged;
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
  const terminalOpen = core.state.sidePanel === "terminal";

  if (due(core.last.history, HISTORY_INTERVAL)) {
    core.last.history = Date.now();
    if (target.kind === "session") {
      try {
        const session = await core.client!.session(target.id);
        core.update((state) => {
          if (sameTarget(state.open, target)) state.detail.session = session;
        });
      } catch {
        // 会话可能已被删除：列表刷新会移除它
      }
    } else {
      try {
        const workflow = await core.client!.workflow(target.id);
        core.update((state) => {
          if (sameTarget(state.open, target)) state.detail.workflow = workflow;
        });
      } catch {
        // 同上
      }
    }
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
  if (due(core.last.options, OPTIONS_INTERVAL) && target.kind === "session") {
    core.last.options = Date.now();
    try {
      const [options, commands, context] = await Promise.all([
        core.client!.configOptions(target.id),
        core.client!.slashCommands(target.id),
        core.client!.context(target.id),
      ]);
      core.update((state) => {
        if (!sameTarget(state.open, target)) return;
        state.detail.configOptions = options;
        state.detail.slashCommands = commands;
        state.detail.contextSize = context.contextSize;
        state.detail.contextWindowSize = context.contextWindowSize;
      });
    } catch {
      // 会话选项需 agent 侧就绪；失败留待下一周期
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
  if (terminalOpen && target.kind === "session" && due(core.last.terminal, TERMINAL_INTERVAL)) {
    core.last.terminal = Date.now();
    await refreshTerminal(core, target.id);
  }
}

function sameTarget(a: OpenTarget | null, b: OpenTarget): boolean {
  return a !== null && a.kind === b.kind && a.id === b.id;
}

/** 对话历史刷新：拉取最新一段并与窗口合并。 */
export async function refreshHistory(core: Core): Promise<void> {
  const target = core.state.open;
  if (!target) return;
  const limit = refreshLimit(
    core.state.detail.history.length,
    core.state.detail.historyPaging.pageSize,
  );
  try {
    const page =
      target.kind === "session"
        ? await core.client!.history(target.id, limit, 0)
        : await core.client!.workflowHistory(target.id, limit, 0);
    core.update((state) => {
      if (!sameTarget(state.open, target)) return;
      const merged = mergeNewest<HistoryItem>(
        state.detail.history,
        state.detail.historyPaging,
        page.items,
        page.hasMore,
        (item) => item.id,
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
      state.detail.history = merged.items;
      state.detail.historyPaging = { ...merged.paging, hasOlder: page.hasMore };
    });
  } catch (error) {
    core.update((state) => {
      state.detail.historyPaging = { ...state.detail.historyPaging, loadingOlder: false };
    });
    core.failure(`加载更早对话失败：${messageOf(error)}`);
  }
}

/** 活动历史刷新：拉取最新一段并与窗口合并。 */
export async function refreshActivities(core: Core): Promise<void> {
  const target = core.state.open;
  if (!target) return;
  const limit = refreshLimit(
    core.state.detail.activities.length,
    core.state.detail.activitiesPaging.pageSize,
  );
  try {
    const page =
      target.kind === "session"
        ? await core.client!.activities(target.id, limit, 0)
        : await core.client!.workflowActivities(target.id, limit, 0);
    core.update((state) => {
      if (!sameTarget(state.open, target)) return;
      const merged = mergeNewest<Activity>(
        state.detail.activities,
        state.detail.activitiesPaging,
        page.activities,
        page.hasMore,
        (item) => item.id,
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
      state.detail.activities = merged.items;
      state.detail.activitiesPaging = { ...merged.paging, hasOlder: page.hasMore };
    });
  } catch (error) {
    core.update((state) => {
      state.detail.activitiesPaging = { ...state.detail.activitiesPaging, loadingOlder: false };
    });
    core.failure(`加载更早活动失败：${messageOf(error)}`);
  }
}

/** 终端：拉取终端列表，并按游标取活动终端的增量输出。 */
async function refreshTerminal(core: Core, sessionId: string): Promise<void> {
  const client = core.client;
  if (!client) return;
  try {
    const terminals = await client.terminals(sessionId);
    core.update((state) => {
      state.detail.terminals = terminals;
      // 列表中消失的终端（会话重开、退出后被清理）从应用侧移除
      if (
        state.detail.activeTerminal !== null &&
        !terminals.some((terminal) => terminal.id === state.detail.activeTerminal)
      ) {
        state.detail.activeTerminal = null;
        state.detail.stream = { cursor: null };
      }
    });
  } catch {
    // 忽略：下一周期重试
  }
  const active = core.state.detail.activeTerminal;
  if (active === null) return;
  try {
    const output = await client.terminalOutput(sessionId, active, core.state.detail.stream.cursor);
    const next = appendTerminalOutput(output);
    if (next.bytes.length === 0 && !next.reset) return;
    core.update((state) => {
      if (state.detail.activeTerminal !== active) return;
      state.detail.stream = next.stream;
      state.detail.chunk = { seq: state.detail.chunk.seq + 1, bytes: next.bytes, reset: next.reset };
    });
  } catch {
    // 忽略：下一周期重试
  }
}

// ---------- 视图打开时的实时拉取 ----------

/** 新建会话视图：机器、agents 与常用工作目录。 */
export async function refreshNewSession(core: Core): Promise<void> {
  await Promise.all([refreshMachines(core), refreshRecentWorkspaces(core)]);
}

/** 工作流模式所需数据：编排智能体配置（未配置时引导去设置）与已保存计划。 */
export async function refreshWorkflowSetup(core: Core): Promise<void> {
  await Promise.all([refreshOrchestrator(core), refreshPlans(core)]);
}

/** 会话交互视图常驻数据：机器/agents（可用性标记）、编排智能体配置与快捷指令。 */
export async function refreshInteraction(core: Core): Promise<void> {
  await Promise.all([refreshMachines(core), refreshOrchestrator(core), refreshQuickCommands(core)]);
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
      return refreshQuickCommands(core);
    case "skills":
      return refreshSkills(core);
    case "plans":
      return refreshPlans(core);
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
    // 常用工作目录仅为便利信息，失败不打扰用户
  }
}

export async function refreshOrchestrator(core: Core): Promise<void> {
  const client = core.client;
  if (!client) return;
  try {
    const orchestrator = await client.orchestrator();
    core.update((state) => {
      state.settings.orchestrator = orchestrator;
      state.settings.orchestratorLoaded = true;
    });
  } catch {
    // 未配置或读取失败：保持未加载状态，由视图决定是否提示
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

/** 会话详情视图：打开时刷新一次（不定时刷新）。 */
export async function refreshDetails(core: Core): Promise<void> {
  const client = core.client;
  const target = core.state.open;
  if (!client || !target) return;
  try {
    if (target.kind === "session") {
      const session = await client.session(target.id);
      core.update((state) => {
        state.detail.session = session;
      });
    } else {
      const workflow = await client.workflow(target.id);
      core.update((state) => {
        state.detail.workflow = workflow;
      });
    }
  } catch (error) {
    core.failure(`读取会话详情失败：${messageOf(error)}`);
  }
}
