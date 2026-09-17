// 会话列表：普通会话与工作流会话统一按最近活跃排序，工作流会话可展开其关联普通会话
// （docs/PRD.md「会话列表视图」）。

import { entryId, entryUpdatedAt, type ListEntry, type Session, type Workflow } from "./types";
import type { Paging } from "./paging";

/** 按最近活跃倒序排列（同刻按标识稳定排序，避免抖动）。 */
export function sortEntries(entries: readonly ListEntry[]): ListEntry[] {
  return [...entries].sort((a, b) => {
    const delta = entryUpdatedAt(b) - entryUpdatedAt(a);
    return delta !== 0 ? delta : entryId(a).localeCompare(entryId(b));
  });
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
  hasMore: boolean,
): { entries: ListEntry[]; paging: Paging } {
  const page: ListEntry[] = [
    ...sessions.map((session): ListEntry => ({ kind: "session", session })),
    ...workflows.map((workflow): ListEntry => ({ kind: "workflow", workflow })),
  ];
  return { entries: sortEntries(page), paging: { ...paging, hasOlder: hasMore } };
}

/** 列表行：工作流会话展开后其关联普通会话紧随其后（深度 1），不影响其他条目的位置。 */
export type ListRow = { entry: ListEntry; depth: number };

/**
 * 展开状态下的可见行序列。
 *
 * 关联普通会话按自身最近活跃倒序，且不参与顶层排序（工作流会话位置不变）。
 */
export function listRows(entries: readonly ListEntry[], expanded: ReadonlySet<string>): ListRow[] {
  const rows: ListRow[] = [];
  for (const entry of entries) {
    rows.push({ entry, depth: 0 });
    if (entry.kind !== "workflow" || !expanded.has(entry.workflow.id)) continue;
    const linked = [...entry.workflow.linkedSessions].sort((a, b) => b.updatedAt - a.updatedAt);
    for (const session of linked) {
      rows.push({ entry: { kind: "session", session }, depth: 1 });
    }
  }
  return rows;
}

/** 工作流会话是否可展开（有关联普通会话）。 */
export function canExpand(entry: ListEntry): boolean {
  return entry.kind === "workflow" && entry.workflow.linkedSessions.length > 0;
}
