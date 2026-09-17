// 会话列表视图（docs/PRD.md「会话列表视图」、docs/DESIGN.md「会话列表视图」）。
//
// 顶部「+」进入新建会话视图；普通会话与工作流会话统一排序，工作流会话可展开关联普通会话。
// 会话按最近活跃倒序排列（最新在上），滚到最下方时按分页模型加载更早一页。

import { useRef, useState, type UIEvent } from "react";
import { ChevronDown, ChevronRight, Loader2, Plus } from "lucide-react";

import { ConfirmDialog } from "../components/ConfirmDialog";
import { ContextMenu, type MenuState } from "../components/ContextMenu";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { deleteEntry, openEntry, renameEntry, showNewSession, toggleExpand } from "../core/actions";
import { loadOlderList } from "../core/poll";
import { useCore, useCoreState } from "../core/store";
import { canExpand, listRows } from "../lib/list";
import { pageSizeForViewport } from "../lib/paging";
import { entryId, entryState, entryTitle, type ListEntry } from "../lib/types";
import { cn } from "../lib/utils";

export function SessionListPanel() {
  const core = useCore();
  const state = useCoreState();
  const scrollRef = useRef<HTMLDivElement>(null);
  /** 本次行内重命名是否已由按键结束：结束时不再由 blur 重复提交 */
  const handledRef = useRef(false);
  const [menu, setMenu] = useState<MenuState | null>(null);
  // 同一会话可能同时出现在顶层与工作流会话之下，因此以（种类, 标识, 深度）标识正在重命名的行
  const [renaming, setRenaming] = useState<{
    kind: ListEntry["kind"];
    id: string;
    depth: number;
    value: string;
  } | null>(null);
  const [deleting, setDeleting] = useState<ListEntry | null>(null);

  const commitRename = (entry: ListEntry, value: string): void => {
    setRenaming(null);
    void renameEntry(core, entry, value);
  };

  const handleScroll = (event: UIEvent<HTMLDivElement>): void => {
    const node = event.currentTarget;
    const pageSize = pageSizeForViewport(node.clientHeight, node.scrollHeight, state.entries.length);
    if (pageSize !== state.listPaging.pageSize) {
      core.update((next) => {
        next.listPaging.pageSize = pageSize;
      });
    }
    const nearBottom = node.scrollTop + node.clientHeight >= node.scrollHeight - 8;
    if (nearBottom && state.listPaging.hasOlder && !state.listPaging.loadingOlder) {
      void loadOlderList(core);
    }
  };

  const rows = listRows(state.entries, new Set(state.expanded));

  return (
    <div data-slot="session-list-panel" className="flex h-full min-h-0 flex-col">
      <div className="flex items-center border-b border-border p-2">
        <Button
          data-slot="session-new"
          aria-label="新建会话"
          variant="ghost"
          size="icon"
          onClick={() => showNewSession(core)}
        >
          <Plus />
        </Button>
      </div>
      <div
        ref={scrollRef}
        data-slot="session-list"
        className="flex min-h-0 flex-1 flex-col gap-0.5 overflow-y-auto p-2"
        onScroll={handleScroll}
      >
        {rows.length === 0 ? (
          <p className="py-4 text-center text-sm text-muted-foreground">暂无会话</p>
        ) : (
          rows.map((row) => {
            const entry = row.entry;
            const id = entryId(entry);
            const title = entryTitle(entry) || "未命名会话";
            const active =
              state.open !== null && state.open.kind === entry.kind && state.open.id === id;
            const expanded =
              entry.kind === "workflow" && state.expanded.includes(entry.workflow.id);
            const editing =
              renaming !== null &&
              renaming.kind === entry.kind &&
              renaming.id === id &&
              renaming.depth === row.depth;
            return (
              <div
                key={`${entry.kind}-${id}-${row.depth}`}
                data-slot="session-item"
                data-kind={entry.kind}
                data-depth={row.depth}
                data-active={active ? "true" : "false"}
                onClick={() => void openEntry(core, entry)}
                onContextMenu={(event) => {
                  event.preventDefault();
                  setMenu({
                    x: event.clientX,
                    y: event.clientY,
                    items: [
                      {
                        label: "重命名",
                        onSelect: () => {
                          handledRef.current = false;
                          setRenaming({ kind: entry.kind, id, depth: row.depth, value: title });
                        },
                      },
                      { label: "删除", danger: true, onSelect: () => setDeleting(entry) },
                    ],
                  });
                }}
                className={cn(
                  "flex cursor-pointer items-center gap-2 rounded-md px-2 py-1.5",
                  row.depth === 1 && "pl-6",
                  active ? "bg-accent" : "hover:bg-accent",
                )}
              >
                {entry.kind === "workflow" && canExpand(entry) ? (
                  <button
                    type="button"
                    data-slot="session-expand"
                    data-expanded={expanded ? "true" : "false"}
                    aria-label={expanded ? "折叠关联普通会话" : "展开关联普通会话"}
                    onClick={(event) => {
                      event.stopPropagation();
                      toggleExpand(core, entry.workflow.id);
                    }}
                    className="flex size-4 shrink-0 cursor-pointer items-center justify-center rounded-sm text-muted-foreground hover:text-foreground"
                  >
                    {expanded ? (
                      <ChevronDown className="size-4" />
                    ) : (
                      <ChevronRight className="size-4" />
                    )}
                  </button>
                ) : null}
                <div className="min-w-0 flex-1">
                  {editing ? (
                    <Input
                      data-slot="session-rename-input"
                      aria-label="会话标题"
                      autoFocus
                      value={renaming.value}
                      onClick={(event) => event.stopPropagation()}
                      onChange={(event) =>
                        setRenaming({
                          kind: entry.kind,
                          id,
                          depth: row.depth,
                          value: event.target.value,
                        })
                      }
                      onKeyDown={(event) => {
                        if (event.key === "Enter") {
                          event.preventDefault();
                          handledRef.current = true;
                          commitRename(entry, renaming.value);
                        } else if (event.key === "Escape") {
                          handledRef.current = true;
                          setRenaming(null);
                        }
                      }}
                      onBlur={() => {
                        if (handledRef.current) {
                          handledRef.current = false;
                          return;
                        }
                        commitRename(entry, renaming.value);
                      }}
                      className="h-6"
                    />
                  ) : (
                    <>
                      <div data-slot="session-title" className="truncate text-sm">
                        {title}
                      </div>
                      {row.depth === 1 && entry.kind === "session" ? (
                        <div className="truncate text-xs text-muted-foreground">
                          {entry.session.agent}@{entry.session.machine}
                        </div>
                      ) : null}
                    </>
                  )}
                </div>
                {entryState(entry) === "busy" ? (
                  <Loader2
                    data-slot="session-spinner"
                    aria-label="工作中"
                    className="size-4 shrink-0 animate-spin text-muted-foreground"
                  />
                ) : null}
              </div>
            );
          })
        )}
      </div>
      <ContextMenu state={menu} onClose={() => setMenu(null)} />
      <ConfirmDialog
        open={deleting !== null}
        title="删除会话"
        description="删除后不可恢复；删除工作流会话会同时删除其关联普通会话。"
        confirmLabel="删除"
        onCancel={() => setDeleting(null)}
        onConfirm={() => {
          const entry = deleting;
          setDeleting(null);
          if (entry !== null) void deleteEntry(core, entry);
        }}
      />
    </div>
  );
}
