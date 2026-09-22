// 会话列表视图（docs/PRD.md「会话列表视图」、docs/DESIGN.md「会话列表视图」）。
//
// 顶部「+」进入新建会话视图；普通会话与工作流会话统一排序，工作流会话可展开关联普通会话。
// 会话按创建时间倒序排列（最新在上）；未归属会话区域滚到最下方时加载更早一页。

import {
  Fragment,
  useCallback,
  useEffect,
  useRef,
  useState,
  type ReactElement,
  type UIEvent,
} from "react";
import {
  ChevronDown,
  ChevronRight,
  Loader2,
  MoreHorizontal,
  Network,
  Plus,
  SquareTerminal,
} from "lucide-react";

import { ConfirmDialog } from "../components/ConfirmDialog";
import { ContextMenu, type MenuItem, type MenuState } from "../components/ContextMenu";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import {
  deleteEntry,
  openEntry,
  renameEntry,
  setEntryProject,
  showNewSession,
  toggleExpand,
} from "../core/actions";
import { useCore, useCoreState } from "../core/store";
import { buildProjectGroupWindow, canExpand, listRows } from "../lib/list";
import {
  entryId,
  entryState,
  entryTitle,
  type ListEntry,
} from "../lib/types";
import { cn } from "../lib/utils";

/** 普通项目组首页 5 条，未归属会话首页 20 条（docs/PRD.md「会话列表视图」）。 */
const PROJECT_GROUP_PAGE = 5;
const UNASSIGNED_GROUP_PAGE = 20;

function projectPageSize(project: string | undefined): number {
  return project === undefined ? UNASSIGNED_GROUP_PAGE : PROJECT_GROUP_PAGE;
}

/**
 * `onNavigate`：窄视口下会话列表在抽屉浮层里，选中会话或新建会话后要收起它，
 * 否则抽屉会盖住刚切换过来的视图。宽视口下抽屉本就不展开，收起动作没有副作用。
 */
export function SessionListPanel({ onNavigate }: { onNavigate: () => void }) {
  const core = useCore();
  const state = useCoreState();
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
  const [collapsedGroups, setCollapsedGroups] = useState<ReadonlySet<string>>(new Set());
  const [groupData, setGroupData] = useState<
    Record<string, { entries: ListEntry[]; loaded: number; hasMore: boolean; loading: boolean }>
  >({});
  const loadedCountsRef = useRef<Record<string, number>>({});
  const loadingGroupsRef = useRef(new Set<string>());
  const [dragEntry, setDragEntry] = useState<ListEntry | null>(null);

  const toggleGroup = (key: string): void => {
    setCollapsedGroups((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };

  const loadGroup = useCallback(
    async (key: string, project: string | undefined, limit: number): Promise<void> => {
      const client = core.client;
      if (client === null || loadingGroupsRef.current.has(key)) return;
      loadingGroupsRef.current.add(key);
      setGroupData((current) => ({
        ...current,
        [key]: {
          entries: current[key]?.entries ?? [],
          loaded: current[key]?.loaded ?? 0,
          hasMore: current[key]?.hasMore ?? false,
          loading: true,
        },
      }));
      try {
        const [sessions, workflows] = await Promise.all([
          client.sessions(limit, 0, project ?? ""),
          client.workflows(limit, 0, project ?? ""),
        ]);
        const window = buildProjectGroupWindow(
          sessions.sessions,
          workflows.workflows,
          limit,
          sessions.hasMore,
          workflows.hasMore,
        );
        loadedCountsRef.current[key] = limit;
        setGroupData((current) => ({
          ...current,
          [key]: {
            entries: window.entries,
            loaded: limit,
            hasMore: window.hasMore,
            loading: false,
          },
        }));
      } catch (error) {
        setGroupData((current) => ({
          ...current,
          [key]: {
            entries: current[key]?.entries ?? [],
            loaded: current[key]?.loaded ?? 0,
            hasMore: current[key]?.hasMore ?? false,
            loading: false,
          },
        }));
        core.failure(`加载项目会话失败：${error instanceof Error ? error.message : String(error)}`);
      } finally {
        loadingGroupsRef.current.delete(key);
      }
    },
    [core],
  );

  const showMore = (group: { key: string; project: string | undefined }): void => {
    const current = groupData[group.key];
    const page = projectPageSize(group.project);
    void loadGroup(group.key, group.project, (current?.loaded ?? page) + page);
  };

  /** 未归属会话区域滚到列表底部时加载下一页；普通项目组仍由「显示更多」推进。 */
  const handleScroll = (event: UIEvent<HTMLDivElement>): void => {
    const node = event.currentTarget;
    if (node.scrollTop + node.clientHeight < node.scrollHeight - 8) return;
    const current = groupData["project:"];
    if (current === undefined || current.loading || !current.hasMore) return;
    void loadGroup("project:", undefined, current.loaded + 20);
  };

  const renderRow = (row: { entry: ListEntry; depth: number }): ReactElement => {
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
        draggable
        onDragStart={(event) => {
          event.dataTransfer.effectAllowed = "move";
          setDragEntry(entry);
        }}
        onDragEnd={() => setDragEntry(null)}
        onClick={() => {
          onNavigate();
          if (entry.kind === "workflow" && canExpand(entry)) {
            toggleExpand(core, entry.workflow.id);
          }
          void openEntry(core, entry);
        }}
        onContextMenu={(event) => {
          event.preventDefault();
          setMenu({
            x: event.clientX,
            y: event.clientY,
            items: menuItems(entry, id, title, row.depth),
          });
        }}
        className={cn(
          "flex cursor-pointer items-center gap-2 rounded-md px-2 py-2 lg:py-1.5",
          row.depth === 1 && "pl-6",
          active ? "bg-accent" : "hover:bg-accent",
        )}
      >
        {entry.kind === "workflow" ? (
          <Network className="size-4 shrink-0 text-muted-foreground" />
        ) : (
          <SquareTerminal className="size-4 shrink-0 text-muted-foreground" />
        )}
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
              // PRD「会话列表视图」：失焦取消，不保存
              onBlur={() => {
                if (handledRef.current) {
                  handledRef.current = false;
                  return;
                }
                setRenaming(null);
              }}
              className="h-9 lg:h-6"
            />
          ) : (
            <div data-slot="session-title" className="overflow-hidden whitespace-nowrap text-sm">
              {title}
            </div>
          )}
        </div>
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
            className="flex size-8 shrink-0 cursor-pointer items-center justify-center rounded-sm text-muted-foreground hover:text-foreground lg:size-4"
          >
            {expanded ? <ChevronDown className="size-4" /> : <ChevronRight className="size-4" />}
          </button>
        ) : null}
        {entryState(entry) === "busy" ? (
          <Loader2
            data-slot="session-spinner"
            aria-label="工作中"
            className="size-4 shrink-0 animate-spin text-muted-foreground"
          />
        ) : null}
        <button
          type="button"
          data-slot="session-menu"
          aria-label="会话操作"
          onClick={(event) => {
            event.stopPropagation();
            const box = event.currentTarget.getBoundingClientRect();
            setMenu({
              x: box.right,
              y: box.bottom,
              items: menuItems(entry, id, title, row.depth),
            });
          }}
          className="flex size-8 shrink-0 items-center justify-center rounded-sm text-muted-foreground hover:bg-accent hover:text-foreground lg:hidden"
        >
          <MoreHorizontal className="size-4" />
        </button>
      </div>
    );
  };


  const commitRename = (entry: ListEntry, value: string): void => {
    setRenaming(null);
    void renameEntry(core, entry, value);
  };

  /** 会话操作菜单项：右键与窄视口的「会话操作」按钮共用。 */
  const menuItems = (
    entry: ListEntry,
    id: string,
    title: string,
    depth: number,
  ): MenuItem[] => [
    {
      label: "重命名",
      onSelect: () => {
        handledRef.current = false;
        setRenaming({ kind: entry.kind, id, depth, value: title });
      },
    },
    { label: "删除", danger: true, onSelect: () => setDeleting(entry) },
  ];

  // 项目组按配置顺序展示；未归属项目不单独成组（docs/PRD.md「会话列表视图」）。
  const projectGroups = state.settings.projects.map((project) => ({
    key: `project:${project.name}`,
    name: project.name,
    label:
      project.description.trim() === ""
        ? project.name
        : `${project.name} · ${project.description}`,
    project: project.name,
  }));
  const projectsKey = projectGroups.map((group) => `${group.project}:${group.key}`).join("|");
  useEffect(() => {
    const loadGroups = [
      ...state.settings.projects.map((project) => ({
        key: `project:${project.name}`,
        project: project.name as string | undefined,
      })),
      { key: "project:", project: undefined },
    ];
    for (const group of loadGroups) {
      if (collapsedGroups.has(group.key)) continue;
      void loadGroup(
        group.key,
        group.project,
        loadedCountsRef.current[group.key] ?? projectPageSize(group.project),
      );
    }
  }, [collapsedGroups, loadGroup, projectsKey, state.entries, state.settings.projects]);

  const visibleProjectGroups = projectGroups.map((group) => {
    const collapsed = collapsedGroups.has(group.key);
    const data = groupData[group.key];
    const visibleEntries = collapsed ? [] : (data?.entries ?? []);
    return {
      ...group,
      collapsed,
      rows: listRows(visibleEntries, new Set(state.expanded)),
      loading: data?.loading ?? !collapsed,
      hasMore: data?.hasMore ?? false,
    };
  });
  const unassignedData = groupData["project:"];
  const unassignedRows = listRows(unassignedData?.entries ?? [], new Set(state.expanded));
  const unassignedLoading = unassignedData?.loading ?? true;
  const hasProjects = state.settings.projects.length > 0;
  const showUnassigned = hasProjects || unassignedLoading || unassignedRows.length > 0;

  return (
    <div data-slot="session-list-panel" className="flex h-full min-h-0 flex-col">
      <div className="flex items-center justify-between gap-2 border-b border-border p-2">
        <h2 className="font-semibold">会话列表</h2>
        <Button
          data-slot="session-new"
          aria-label="新建会话"
          variant="ghost"
          size="icon"
          onClick={() => {
            onNavigate();
            showNewSession(core);
          }}
        >
          <Plus />
        </Button>
      </div>
      <div
        data-slot="session-list"
        className="flex min-h-0 flex-1 flex-col gap-0.5 overflow-y-auto p-2"
        onScroll={handleScroll}
      >
        {visibleProjectGroups.length === 0 && !showUnassigned ? (
          <p className="py-4 text-center text-sm text-muted-foreground">暂无会话</p>
        ) : (
          <>
            {visibleProjectGroups.map((group) => (
              <div
                key={group.key}
                data-slot="session-project-group"
                data-project={group.name}
                onDragOver={(event) => event.preventDefault()}
                onDrop={(event) => {
                  event.preventDefault();
                  if (dragEntry !== null) {
                    const entry = dragEntry;
                    setDragEntry(null);
                    void setEntryProject(core, entry, group.project);
                  }
                }}
                className="flex flex-col gap-0.5"
              >
                <button
                  type="button"
                  data-slot="session-project-group-header"
                  aria-expanded={!group.collapsed}
                  onClick={() => toggleGroup(group.key)}
                  className="flex cursor-pointer items-center gap-1 rounded-md px-1 py-1 text-sm font-semibold text-foreground hover:bg-accent"
                >
                  {group.collapsed ? (
                    <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />
                  ) : (
                    <ChevronDown className="size-3.5 shrink-0 text-muted-foreground" />
                  )}
                  <span className="min-w-0 flex-1 truncate">{group.label}</span>
                </button>
                {!group.collapsed && group.loading && group.rows.length === 0 ? (
                  <p className="px-2 py-1 text-xs text-muted-foreground">加载中…</p>
                ) : null}
                {!group.collapsed
                  ? group.rows.map((row, index) => {
                      const element = renderRow(row);
                      return <Fragment key={index}>{element}</Fragment>;
                    })
                  : null}
                {!group.collapsed && group.hasMore ? (
                  <button
                    type="button"
                    data-slot="session-project-show-more"
                    onClick={() => showMore(group)}
                    className="cursor-pointer rounded-md px-2 py-1 text-center text-xs text-muted-foreground hover:bg-accent"
                  >
                    显示更多
                  </button>
                ) : null}
              </div>
            ))}
            {showUnassigned ? (
              <div
                data-slot="session-unassigned"
                onDragOver={(event) => event.preventDefault()}
                onDrop={(event) => {
                  event.preventDefault();
                  if (dragEntry !== null) {
                    const entry = dragEntry;
                    setDragEntry(null);
                    void setEntryProject(core, entry, undefined);
                  }
                }}
                className="flex min-h-6 flex-col gap-0.5"
              >
                {hasProjects ? (
                  <div
                    data-slot="session-project-separator"
                    className="my-1 border-t border-border"
                  />
                ) : null}
                {unassignedLoading && unassignedRows.length === 0 ? (
                  <p className="px-2 py-1 text-xs text-muted-foreground">加载中…</p>
                ) : null}
                {unassignedRows.map((row, index) => {
                  const element = renderRow(row);
                  return <Fragment key={index}>{element}</Fragment>;
                })}
                {unassignedLoading && unassignedRows.length > 0 ? (
                  <p className="px-2 py-1 text-center text-xs text-muted-foreground">加载中…</p>
                ) : null}
              </div>
            ) : null}
          </>
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
