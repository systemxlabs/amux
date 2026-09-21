// 改动审查视图：改动文件树 + inline 改动（docs/PRD.md「改动审查视图」、docs/DESIGN.md「改动审查视图」）。
//
// 面板打开时拉取一次改动，不定时刷新；折叠状态只存在于本地。

import { useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent, type ReactNode } from "react";

import { ResizableTreePane } from "../../components/ResizableTreePane";
import { Button } from "../../components/ui/button";
import { sendPrompt } from "../../core/actions";
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

type DiffCommentTarget =
  | { kind: "file"; path: string }
  | { kind: "code"; path: string; hunkIndex: number; code: string };

type DiffLineRef = { path: string; hunkIndex: number; lineIndex: number };
type DiffDrag = { start: DiffLineRef; end: DiffLineRef };

function sameHunk(a: DiffLineRef, b: DiffLineRef): boolean {
  return a.path === b.path && a.hunkIndex === b.hunkIndex;
}

function commentMessage(target: DiffCommentTarget, comment: string): string {
  if (target.kind === "file") return `${target.path} ${comment}`;
  return `\`\`\`\n${target.code}\n\`\`\`\n${comment}`;
}

function CommentComposer({
  target,
  value,
  sending,
  onChange,
  onCancel,
  onSubmit,
}: {
  target: DiffCommentTarget;
  value: string;
  sending: boolean;
  onChange: (value: string) => void;
  onCancel: () => void;
  onSubmit: () => void;
}) {
  const label =
    target.kind === "file" ? `评论 ${target.path}` : `评论 ${target.path} 中的选中代码`;
  return (
    <div data-slot="diff-comment-composer" className="border-b border-border bg-card p-2">
      <div className="mb-1 truncate text-xs text-muted-foreground">{label}</div>
      <textarea
        data-slot="diff-comment-input"
        aria-label="评论内容"
        autoFocus
        rows={3}
        value={value}
        disabled={sending}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && !event.shiftKey) {
            event.preventDefault();
            if (value.trim() !== "") onSubmit();
          } else if (event.key === "Escape") {
            event.preventDefault();
            onCancel();
          }
        }}
        className="w-full resize-y rounded-md border border-input bg-background px-2 py-1.5 font-mono text-xs outline-none focus-visible:ring-2 focus-visible:ring-ring"
      />
      <div className="mt-2 flex justify-end gap-2">
        <Button type="button" variant="ghost" size="sm" disabled={sending} onClick={onCancel}>
          取消
        </Button>
        <Button
          type="button"
          size="sm"
          disabled={sending || value.trim() === ""}
          onClick={onSubmit}
        >
          {sending ? "发送中…" : "评论"}
        </Button>
      </div>
    </div>
  );
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
  /** 折叠全部文件改动：折叠后仅显示文件名（docs/PRD.md「改动审查视图」） */
  const [diffsCollapsed, setDiffsCollapsed] = useState(false);
  const [collapsedDirs, setCollapsedDirs] = useState<string[]>([]);
  const [selected, setSelected] = useState<number | null>(null);
  const [commentTarget, setCommentTarget] = useState<DiffCommentTarget | null>(null);
  const [commentText, setCommentText] = useState("");
  const [commentSending, setCommentSending] = useState(false);
  const [dragRange, setDragRange] = useState<DiffDrag | null>(null);
  const dragRef = useRef<DiffDrag | null>(null);

  // 打开时刷新一次（docs/DESIGN.md「改动审查视图」）：会话切换时重新拉取
  useEffect(() => {
    setDiff(null);
    setError(null);
    setDiffsCollapsed(false);
    setCollapsedDirs([]);
    setSelected(null);
    setCommentTarget(null);
    setCommentText("");
    setCommentSending(false);
    setDragRange(null);
    dragRef.current = null;
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

  const openFileComment = (path: string) => {
    setCommentTarget({ kind: "file", path });
    setCommentText("");
  };

  const submitComment = async () => {
    const target = commentTarget;
    const comment = commentText.trim();
    if (target === null || comment === "" || commentSending) return;
    setCommentSending(true);
    const ok = await sendPrompt(core, commentMessage(target, comment), false);
    setCommentSending(false);
    if (ok) {
      setCommentTarget(null);
      setCommentText("");
    }
  };

  const cancelComment = () => {
    setCommentTarget(null);
    setCommentText("");
    setDragRange(null);
    dragRef.current = null;
  };

  const lineRefAt = (event: ReactPointerEvent<HTMLDivElement>): DiffLineRef | null => {
    const direct = (event.target as HTMLElement).closest<HTMLElement>("[data-diff-line]");
    const underPointer = document
      .elementFromPoint(event.clientX, event.clientY)
      ?.closest<HTMLElement>("[data-diff-line]");
    const element = direct ?? underPointer;
    if (element == null) return null;
    const path = element.dataset.diffFile;
    const hunkIndex = Number(element.dataset.diffHunk);
    const lineIndex = Number(element.dataset.diffLineIndex);
    if (path === undefined || Number.isNaN(hunkIndex) || Number.isNaN(lineIndex)) return null;
    return { path, hunkIndex, lineIndex };
  };

  const beginCodeSelection = (event: ReactPointerEvent<HTMLDivElement>) => {
    const line = lineRefAt(event);
    if (line === null) return;
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    const drag = { start: line, end: line };
    dragRef.current = drag;
    setDragRange(drag);
  };

  const extendCodeSelection = (event: ReactPointerEvent<HTMLDivElement>) => {
    const current = dragRef.current;
    if (current === null) return;
    const line = lineRefAt(event);
    if (line === null || !sameHunk(current.start, line)) return;
    const drag = { ...current, end: line };
    dragRef.current = drag;
    setDragRange(drag);
  };

  const finishCodeSelection = (event: ReactPointerEvent<HTMLDivElement>) => {
    extendCodeSelection(event);
    const drag = dragRef.current;
    if (drag === null) return;
    const file = files.find((item) => item.path === drag.start.path);
    const hunk = file?.hunks[drag.start.hunkIndex];
    if (hunk === undefined) {
      cancelComment();
      return;
    }
    const start = Math.min(drag.start.lineIndex, drag.end.lineIndex);
    const end = Math.max(drag.start.lineIndex, drag.end.lineIndex);
    const code = hunk.lines
      .slice(start, end + 1)
      .map((line) => `${linePrefix(line.kind)}${line.text}`)
      .join("\n");
    dragRef.current = null;
    setDragRange(null);
    setCommentTarget({ kind: "code", path: drag.start.path, hunkIndex: drag.start.hunkIndex, code });
    setCommentText("");
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
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
    // 窄视口下文件树与 diff 上下排布；宽屏可通过分割线调整左右宽度
    body = (
      <ResizableTreePane
        label="调整改动文件树与 diff 宽度"
        tree={
          treeVisible
            ? nodes.map((node) => (
                <DiffTreeNode
                  key={node.key}
                  node={node}
                  depth={0}
                  collapsedDirs={collapsedDirs}
                  selected={selected}
                  onToggleDir={toggleDir}
                  onSelectFile={selectFile}
                />
              ))
            : null
        }
        content={
          <div
            data-slot="diff-content"
            className="select-none"
            onPointerDown={beginCodeSelection}
            onPointerMove={extendCodeSelection}
            onPointerUp={finishCodeSelection}
            onPointerCancel={cancelComment}
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
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    data-slot="diff-comment-file"
                    className="h-9 shrink-0 text-xs lg:h-5"
                    onClick={() => openFileComment(file.path)}
                  >
                    评论
                  </Button>
                </div>
                {commentTarget?.kind === "file" && commentTarget.path === file.path ? (
                  <CommentComposer
                    target={commentTarget}
                    value={commentText}
                    sending={commentSending}
                    onChange={setCommentText}
                    onCancel={cancelComment}
                    onSubmit={() => void submitComment()}
                  />
                ) : null}
                {!diffsCollapsed &&
                  file.hunks.map((hunk, hunkIx) => {
                    const selectedLines =
                      dragRange !== null &&
                      dragRange.start.path === file.path &&
                      dragRange.start.hunkIndex === hunkIx
                        ? [
                            Math.min(dragRange.start.lineIndex, dragRange.end.lineIndex),
                            Math.max(dragRange.start.lineIndex, dragRange.end.lineIndex),
                          ]
                        : null;
                    return (
                      <div key={hunkIx}>
                        <div className="flex px-2 font-mono text-xs text-muted-foreground">
                          <span className="min-w-0 flex-1">{hunk.header}</span>
                        </div>
                        {hunk.lines.map((line, lineIx) => (
                          <div
                            key={lineIx}
                            data-slot="diff-line"
                            data-diff-file={file.path}
                            data-diff-hunk={hunkIx}
                            data-diff-line-index={lineIx}
                            className={cn(
                              "cursor-text whitespace-pre px-2 font-mono text-xs",
                              line.kind === "add" && "bg-diff-add",
                              line.kind === "remove" && "bg-diff-remove",
                              selectedLines !== null &&
                                lineIx >= selectedLines[0] &&
                                lineIx <= selectedLines[1] &&
                                "ring-1 ring-inset ring-primary/60",
                            )}
                          >
                            {linePrefix(line.kind)}
                            {line.text}
                          </div>
                        ))}
                        {commentTarget?.kind === "code" &&
                        commentTarget.path === file.path &&
                        commentTarget.hunkIndex === hunkIx ? (
                          <CommentComposer
                            target={commentTarget}
                            value={commentText}
                            sending={commentSending}
                            onChange={setCommentText}
                            onCancel={cancelComment}
                            onSubmit={() => void submitComment()}
                          />
                        ) : null}
                      </div>
                    );
                  })}
              </div>
            ))}
          </div>
        }
        treeSlot="diff-tree"
        contentSlot="diff-files"
        treeClassName="max-h-[40%] min-h-0 overflow-y-auto rounded-md bg-muted/40 p-1 lg:h-full lg:max-h-none"
        contentClassName="min-h-0 flex-1 overflow-auto rounded-md bg-muted/20 lg:h-full"
        contentCollapsed={diffsCollapsed}
      />
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      {/* 工具栏（docs/PRD.md「改动审查视图」）：折叠/展开文件树按钮左对齐，
          折叠/展开 diff 区域按钮右对齐（折叠后仅显示文件名） */}
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
        <Button
          type="button"
          variant="ghost"
          size="sm"
          data-slot="diff-toggle-changes"
          onClick={() => setDiffsCollapsed((prev) => !prev)}
        >
          {diffsCollapsed ? "展开 diff 区域" : "折叠 diff 区域"}
        </Button>
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
