// 工作目录视图：文件树 + 文件内容（docs/PRD.md「工作目录视图」、docs/DESIGN.md「工作目录视图」）。
//
// 每一级目录项都在节点展开时实时拉取，不做缓存也不定时刷新；文件内容按行拉取一次。

import { useCallback, useEffect, useState } from "react";

import { Button } from "../../components/ui/button";
import { useCore, useCoreState } from "../../core/store";
import type { FsEntry } from "../../lib/types";
import { rootDir } from "../../lib/types";
import { cn } from "../../lib/utils";
import { isInSubtree } from "../../lib/workspace";

/** 目录页大小（与服务端夹取后的上限一致）。 */
const DIR_PAGE_LIMIT = 500;
/** 单次读取的行数；超出部分不展示。 */
const FILE_LINE_LIMIT = 400;

type Level = { entries: FsEntry[]; loading: boolean; error: string | null };

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** 目录项排序：目录在前，各自按名称。 */
function sortEntries(entries: readonly FsEntry[]): FsEntry[] {
  return [...entries].sort((a, b) => {
    if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
}

type TreeProps = {
  entries: readonly FsEntry[];
  depth: number;
  levels: Record<string, Level>;
  expanded: readonly string[];
  selected: string | null;
  onToggleDir: (path: string) => void;
  onOpenFile: (entry: FsEntry) => void;
};

function TreeNodes({ entries, depth, levels, expanded, selected, onToggleDir, onOpenFile }: TreeProps) {
  return (
    <>
      {entries.map((entry) => {
        const open = entry.isDir && expanded.includes(entry.path);
        const level = levels[entry.path];
        return (
          <div key={entry.path}>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              data-slot="workspace-node"
              data-dir={entry.isDir ? "true" : "false"}
              className={cn(
                "w-full justify-start font-normal",
                !entry.isDir && selected === entry.path && "bg-accent text-accent-foreground",
              )}
              style={{ paddingLeft: `${8 + depth * 12}px` }}
              onClick={() => (entry.isDir ? onToggleDir(entry.path) : onOpenFile(entry))}
            >
              {/* 展开箭头列：文件不显示符号，但保留同宽占位，使名称与目录对齐 */}
              <span className="w-3 shrink-0 text-muted-foreground">
                {entry.isDir ? (open ? "▾" : "▸") : null}
              </span>
              <span className="truncate">{entry.name}</span>
            </Button>
            {open && (
              <div>
                {level === undefined || level.loading ? (
                  <div
                    className="py-0.5 text-xs text-muted-foreground"
                    style={{ paddingLeft: `${20 + depth * 12}px` }}
                  >
                    加载中…
                  </div>
                ) : level.error !== null ? (
                  <div
                    className="py-0.5 text-xs text-destructive"
                    style={{ paddingLeft: `${20 + depth * 12}px` }}
                  >
                    加载失败：{level.error}
                  </div>
                ) : (
                  <TreeNodes
                    entries={level.entries}
                    depth={depth + 1}
                    levels={levels}
                    expanded={expanded}
                    selected={selected}
                    onToggleDir={onToggleDir}
                    onOpenFile={onOpenFile}
                  />
                )}
              </div>
            )}
          </div>
        );
      })}
    </>
  );
}

export function WorkspacePanel() {
  const core = useCore();
  const state = useCoreState();
  const session = state.detail.session;
  const machine = session?.machine ?? null;
  const root = session === null ? null : rootDir(session);

  const [treeVisible, setTreeVisible] = useState(true);
  const [contentVisible, setContentVisible] = useState(true);
  const [levels, setLevels] = useState<Record<string, Level>>({});
  const [expanded, setExpanded] = useState<string[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [file, setFile] = useState<string | null>(null);
  const [fileLoading, setFileLoading] = useState(false);
  const [fileError, setFileError] = useState<string | null>(null);

  const loadDir = useCallback(
    async (path: string) => {
      const client = core.client;
      if (client === null || machine === null) return;
      setLevels((prev) => ({ ...prev, [path]: { entries: [], loading: true, error: null } }));
      try {
        const result = await client.listDir(machine, path, DIR_PAGE_LIMIT, 0, false);
        setLevels((prev) => ({
          ...prev,
          [path]: { entries: sortEntries(result.entries), loading: false, error: null },
        }));
      } catch (error) {
        setLevels((prev) => ({
          ...prev,
          [path]: { entries: [], loading: false, error: messageOf(error) },
        }));
      }
    },
    [core, machine],
  );

  // 切换会话或根目录变化时整棵树重新拉取
  useEffect(() => {
    setLevels({});
    setExpanded([]);
    setSelected(null);
    setFile(null);
    setFileError(null);
    if (root !== null) void loadDir(root);
  }, [root, loadDir]);

  const toggleDir = (path: string) => {
    const open = expanded.includes(path);
    if (open) {
      // 折叠即丢弃整棵子树（展开状态与已加载目录项）：不保留任何缓存，
      // 再次展开时父目录与逐级子目录都重新拉取（docs/DESIGN.md「工作目录视图」）
      setExpanded((prev) => prev.filter((item) => !isInSubtree(item, path)));
      setLevels((prev) =>
        Object.fromEntries(Object.entries(prev).filter(([key]) => !isInSubtree(key, path))),
      );
      return;
    }
    setExpanded((prev) => [...prev, path]);
    // 展开即重新拉取，不使用任何缓存
    void loadDir(path);
  };

  const openFile = async (entry: FsEntry) => {
    const client = core.client;
    if (client === null || machine === null) return;
    setSelected(entry.path);
    setFile(null);
    setFileError(null);
    setFileLoading(true);
    try {
      const result = await client.readFile(machine, entry.path, FILE_LINE_LIMIT, 0);
      setFile(result.content);
    } catch (error) {
      setFileError(messageOf(error));
    } finally {
      setFileLoading(false);
    }
  };

  const rootLevel = root === null ? undefined : levels[root];

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex shrink-0 items-center justify-between gap-2 px-3 py-2">
        <Button
          type="button"
          variant="ghost"
          size="sm"
          data-slot="workspace-toggle-tree"
          onClick={() => setTreeVisible((prev) => !prev)}
        >
          {treeVisible ? "折叠文件树" : "展开文件树"}
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          data-slot="workspace-toggle-content"
          onClick={() => setContentVisible((prev) => !prev)}
        >
          {contentVisible ? "折叠内容区域" : "展开内容区域"}
        </Button>
      </div>
      <div className="flex min-h-0 flex-1 items-stretch gap-2 px-3 pb-3">
        {treeVisible && (
          <div
            data-slot="workspace-tree"
            className={cn(
              "h-full min-h-0 overflow-y-auto rounded-md bg-muted/40 p-1",
              contentVisible ? "w-1/2" : "flex-1",
            )}
          >
            {session === null || root === null ? (
              <div className="px-2 py-1 text-xs text-muted-foreground">未选择会话</div>
            ) : (
              <>
                <div className="truncate px-2 py-1 text-xs text-muted-foreground">{root}</div>
                {rootLevel === undefined || rootLevel.loading ? (
                  <div className="px-2 py-1 text-xs text-muted-foreground">加载中…</div>
                ) : rootLevel.error !== null ? (
                  <div className="px-2 py-1 text-xs text-destructive">
                    加载失败：{rootLevel.error}
                  </div>
                ) : (
                  <TreeNodes
                    entries={rootLevel.entries}
                    depth={0}
                    levels={levels}
                    expanded={expanded}
                    selected={selected}
                    onToggleDir={toggleDir}
                    onOpenFile={openFile}
                  />
                )}
              </>
            )}
          </div>
        )}
        {contentVisible && (
          <div
            data-slot="workspace-content"
            className="h-full min-h-0 flex-1 overflow-auto rounded-md bg-muted/20 p-1"
          >
            {fileLoading ? (
              <div className="px-2 py-1 text-xs text-muted-foreground">加载中…</div>
            ) : fileError !== null ? (
              <div className="px-2 py-1 text-xs text-destructive">读取文件失败：{fileError}</div>
            ) : file !== null ? (
              <pre className="whitespace-pre p-1 font-mono text-xs">{file}</pre>
            ) : (
              <div className="px-2 py-1 text-xs text-muted-foreground">
                在左侧选择文件查看内容
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
