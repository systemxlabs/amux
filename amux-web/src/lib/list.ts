// 会话列表：置顶优先，其他普通会话与工作流会话按创建时间排序；工作流会话可展开其关联普通会话
// （docs/PRD.md「会话列表视图」）。

import {
  entryCreatedAt,
  entryId,
  entryPinned,
  type Group,
  type ListEntry,
  type Session,
  type Workflow,
} from "./types";
import type { Paging } from "./paging";

/** 置顶条目在前；两端各自按创建时间倒序排列。 */
export function sortEntries(entries: readonly ListEntry[]): ListEntry[] {
  return [...entries].sort((a, b) => {
    const pinned = Number(entryPinned(b)) - Number(entryPinned(a));
    if (pinned !== 0) return pinned;
    const delta = entryCreatedAt(b) - entryCreatedAt(a);
    return delta !== 0 ? delta : entryId(a).localeCompare(entryId(b));
  });
}

/** 列表条目及内容是否完全相同，用于后台刷新时避免无变化的重渲染。 */
export function sameListEntries(
  left: readonly ListEntry[],
  right: readonly ListEntry[],
): boolean {
  return (
    left.length === right.length &&
    left.every((entry, index) => JSON.stringify(entry) === JSON.stringify(right[index]))
  );
}

/**
 * 会话列表刷新窗口：用最新一窗普通会话与工作流会话重建已加载窗口。
 *
 * 两个来源各取首页即可覆盖已加载窗口，窗口整体替换、不保留服务端已删除的条目。
 */
export function buildListWindow(
  paging: Paging,
  sessions: readonly Session[],
  workflows: readonly Workflow[],
  groups: readonly Group[],
  hasMore: boolean,
): { entries: ListEntry[]; paging: Paging } {
  const page: ListEntry[] = [
    ...sessions.map((session): ListEntry => ({ kind: "session", session })),
    ...workflows.map((workflow): ListEntry => ({ kind: "workflow", workflow })),
    ...groups.map((group): ListEntry => ({ kind: "group", group })),
  ];
  return { entries: sortEntries(page), paging: { ...paging, hasOlder: hasMore } };
}

/**
 * 项目组窗口：置顶项全部保留，非置顶项按创建时间合并后截断为项目组的可见条数。
 * 未归属组与普通项目组使用不同的 limit（docs/PRD.md「会话列表视图」）。
 */
export function buildProjectGroupWindow(
  sessions: readonly Session[],
  workflows: readonly Workflow[],
  groups: readonly Group[],
  limit: number,
  sessionsHaveMore: boolean,
  workflowsHaveMore: boolean,
  groupsHaveMore: boolean,
): { entries: ListEntry[]; hasMore: boolean } {
  const entries: ListEntry[] = [
    ...sessions.map((session): ListEntry => ({ kind: "session", session })),
    ...workflows.map((workflow): ListEntry => ({ kind: "workflow", workflow })),
    ...groups.map((group): ListEntry => ({ kind: "group", group })),
  ];
  const pinned = entries
    .filter(entryPinned)
    .sort((a, b) => entryCreatedAt(b) - entryCreatedAt(a) || entryId(a).localeCompare(entryId(b)));
  const unpinned = entries
    .filter((entry) => !entryPinned(entry))
    .sort((a, b) => entryCreatedAt(b) - entryCreatedAt(a) || entryId(a).localeCompare(entryId(b)));
  return {
    entries: [...pinned, ...unpinned.slice(0, limit)],
    hasMore: unpinned.length > limit || sessionsHaveMore || workflowsHaveMore || groupsHaveMore,
  };
}

/** 关联会话之间的树形连接位置。 */
export type ListTreeConnector = "first" | "middle" | "last";

/** 列表行：工作流会话展开后其关联普通会话紧随其后（深度 1），不影响其他条目的位置。 */
export type ListRow = {
  entry: ListEntry;
  depth: number;
  connector?: ListTreeConnector;
};

/**
 * 展开状态下的可见行序列。
 *
 * 关联普通会话置顶优先、其余按创建时间倒序，且不参与顶层排序（工作流会话位置不变）。
 */
export function listRows(entries: readonly ListEntry[], expanded: ReadonlySet<string>): ListRow[] {
  const rows: ListRow[] = [];
  for (const entry of entries) {
    rows.push({ entry, depth: 0 });
    const expandId =
      entry.kind === "workflow"
        ? entry.workflow.id
        : entry.kind === "group"
          ? entry.group.id
          : null;
    if (expandId === null || !expanded.has(expandId)) continue;
    const linkedSessions =
      entry.kind === "workflow"
        ? entry.workflow.linkedSessions
        : entry.kind === "group"
          ? entry.group.sessions
          : [];
    const linked = [...linkedSessions].sort(
      (a, b) =>
        Number(b.pinned) - Number(a.pinned) ||
        b.createdAt - a.createdAt ||
        a.id.localeCompare(b.id),
    );
    for (const [index, session] of linked.entries()) {
      const connector: ListTreeConnector =
        linked.length === 1
          ? "last"
          : index === 0
            ? "first"
            : index === linked.length - 1
              ? "last"
              : "middle";
      rows.push({ entry: { kind: "session", session }, depth: 1, connector });
    }
  }
  return rows;
}

/** 工作流/群聊会话是否可展开（有关联普通会话）。 */
export function canExpand(entry: ListEntry): boolean {
  if (entry.kind === "workflow") return entry.workflow.linkedSessions.length > 0;
  if (entry.kind === "group") return entry.group.sessions.length > 0;
  return false;
}
