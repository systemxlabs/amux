// 改动审查视图：改动文件树 + inline 改动（docs/PRD.md「改动审查视图」、docs/DESIGN.md「改动审查视图」）。
//
// 面板打开时拉取一次改动，不定时刷新；折叠状态只存在于本地。

import { useEffect, useMemo, useState, type ReactNode } from "react";

import { Button } from "../../components/ui/button";
import { useCore, useCoreState } from "../../core/store";
import { buildDiffTree, type DiffNode } from "../../lib/difftree";
import type { GitChangeStatus, GitDiffLine, GitDiffResult } from "../../lib/types";
import { cn } from "../../lib/utils";

const STATUS_LABEL: Record<GitChangeStatus, string> = {
  added: "新增",
  modified: "修改",
  deleted: "删除",
};

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function linePrefix(kind: GitDiffLine["kind"]): string {
  if (kind === "add") return "+";
  if (kind === "remove") return "-";
  return " ";
}

type TreeProps = {
  node: DiffNode;
  depth: number;
  collapsedDirs: readonly string[];
  selected: number | null;
  onToggleDir: (key: string) => void;
  onSelectFile: (index: number) => void;
};

function DiffTreeNode({ node, depth, collapsedDirs, selected, onToggleDir, onSelectFile }: TreeProps) {
  const indent = { paddingLeft: `${8 + depth * 12}px` };
  const index = node.fileIx;
  if (index !== null) {
    return (
      <Button
        type="button"
        variant="ghost"
        size="sm"
        data-slot="diff-node"
        className={cn(
          "w-full justify-start font-normal",
          selected === index && "bg-accent text-accent-foreground",
        )}
        style={indent}
        onClick={() => onSelectFile(index)}
      >
        <span className="truncate">{node.name}</span>
      </Button>
    );
  }
  const collapsed = collapsedDirs.includes(node.key);
  return (
    <div>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        data-slot="diff-node"
        className="w-full justify-start font-normal"
        style={indent}
        onClick={() => onToggleDir(node.key)}
      >
        <span className="shrink-0 text-muted-foreground">{collapsed ? "▸" : "▾"}</span>
        <span className="truncate">{node.name}</span>
      </Button>
      {!collapsed &&
        node.children.map((child) => (
          <DiffTreeNode
            key={child.key}
            node={child}
            depth={depth + 1}
            collapsedDirs={collapsedDirs}
            selected={selected}
            onToggleDir={onToggleDir}
            onSelectFile={onSelectFile}
          />
        ))}
    </div>
  );
}

export function DiffPanel() {
  const core = useCore();
  const state = useCoreState();
  const sessionId = state.open?.kind === "session" ? state.open.id : null;

  const [diff, setDiff] = useState<GitDiffResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [treeVisible, setTreeVisible] = useState(true);
  const [contentVisible, setContentVisible] = useState(true);
  const [collapseAll, setCollapseAll] = useState(false);
  const [collapsedDirs, setCollapsedDirs] = useState<string[]>([]);
  const [selected, setSelected] = useState<number | null>(null);

  // 打开时刷新一次（docs/DESIGN.md「改动审查视图」）：会话切换时重新拉取
  useEffect(() => {
    setDiff(null);
    setError(null);
    setCollapseAll(false);
    setCollapsedDirs([]);
    setSelected(null);
    const client = core.client;
    if (client === null || sessionId === null) return;
    let cancelled = false;
    client.diff(sessionId).then(
      (result) => {
        if (!cancelled) setDiff(result);
      },
      (cause: unknown) => {
        if (!cancelled) setError(messageOf(cause));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [core, sessionId]);

  const files = diff?.files ?? [];
  const nodes = useMemo(() => buildDiffTree(diff?.files ?? []), [diff]);

  const toggleDir = (key: string) => {
    setCollapsedDirs((prev) =>
      prev.includes(key) ? prev.filter((item) => item !== key) : [...prev, key],
    );
  };

  const selectFile = (index: number) => {
    setSelected(index);
    document.getElementById(`diff-file-${index}`)?.scrollIntoView({ block: "start" });
  };

  let body: ReactNode;
  if (error !== null) {
    body = <Hint text={`读取改动失败：${error}`} />;
  } else if (diff === null) {
    body = <Hint text="正在加载改动…" />;
  } else if (diff.notRepo === true) {
    body = <Hint text="当前工作目录不是 git 仓库" />;
  } else if (files.length === 0) {
    body = <Hint text="暂无改动" />;
  } else {
    body = (
      <div className="flex min-h-0 flex-1 items-stretch gap-2 px-3 pb-3">
        {treeVisible && (
          <div
            data-slot="diff-tree"
            className={cn(
              "h-full min-h-0 overflow-y-auto rounded-md bg-muted/40 p-1",
              contentVisible ? "w-1/2" : "flex-1",
            )}
          >
            {nodes.map((node) => (
              <DiffTreeNode
                key={node.key}
                node={node}
                depth={0}
                collapsedDirs={collapsedDirs}
                selected={selected}
                onToggleDir={toggleDir}
                onSelectFile={selectFile}
              />
            ))}
          </div>
        )}
        {contentVisible && (
          <div
            data-slot="diff-files"
            className="h-full min-h-0 flex-1 overflow-auto rounded-md bg-muted/20"
          >
            {files.map((file, index) => (
              <div
                key={file.path}
                id={`diff-file-${index}`}
                data-slot="diff-file"
                className="mb-2 rounded-md border border-border"
              >
                <div className="flex items-center gap-2 border-b border-border bg-muted/40 px-2 py-1 font-mono text-xs">
                  <span className="min-w-0 flex-1 truncate">{file.path}</span>
                  <span className="shrink-0">{STATUS_LABEL[file.status]}</span>
                  <span className="shrink-0 text-muted-foreground">
                    +{file.additions}/-{file.deletions}
                  </span>
                </div>
                {!collapseAll &&
                  file.hunks.map((hunk, hunkIx) => (
                    <div key={hunkIx}>
                      <div className="whitespace-pre px-2 font-mono text-xs text-muted-foreground">
                        {hunk.header}
                      </div>
                      {hunk.lines.map((line, lineIx) => (
                        <div
                          key={lineIx}
                          className={cn(
                            "whitespace-pre px-2 font-mono text-xs",
                            line.kind === "add" && "bg-diff-add",
                            line.kind === "remove" && "bg-diff-remove",
                          )}
                        >
                          {linePrefix(line.kind)}
                          {line.text}
                        </div>
                      ))}
                    </div>
                  ))}
              </div>
            ))}
          </div>
        )}
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex shrink-0 items-center justify-between gap-2 px-3 py-2">
        <Button
          type="button"
          variant="ghost"
          size="sm"
          data-slot="diff-toggle-tree"
          onClick={() => setTreeVisible((prev) => !prev)}
        >
          {treeVisible ? "折叠文件树" : "展开文件树"}
        </Button>
        <div className="flex items-center gap-2">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            data-slot="diff-collapse-all"
            onClick={() => setCollapseAll((prev) => !prev)}
          >
            {collapseAll ? "展开全部 diff" : "折叠全部 diff"}
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            data-slot="diff-toggle-content"
            onClick={() => setContentVisible((prev) => !prev)}
          >
            {contentVisible ? "折叠 diff 区域" : "展开 diff 区域"}
          </Button>
        </div>
      </div>
      {body}
    </div>
  );
}

function Hint({ text }: { text: string }) {
  return (
    <div className="flex flex-1 items-center justify-center px-3 pb-3 text-sm text-muted-foreground">
      {text}
    </div>
  );
}
