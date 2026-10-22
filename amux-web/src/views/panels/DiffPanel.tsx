// 改动审查视图：改动文件树 + inline 改动（docs/PRD.md「改动审查视图」、docs/DESIGN.md「改动审查视图」）。
//
// 面板打开时拉取一次改动，不自动刷新；折叠状态只存在于本地。

import { Fragment, useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent, type ReactNode } from "react";
import {
  ChevronDown,
  PanelLeftClose,
  PanelLeftOpen,
  PanelRightClose,
  PanelRightOpen,
  Plus,
  RefreshCw,
} from "lucide-react";

import { ResizableTreePane } from "../../components/ResizableTreePane";
import { Button } from "../../components/ui/button";
import { Input } from "../../components/ui/input";
import { sendPrompt } from "../../core/actions";
import { useCore, useCoreState } from "../../core/store";
import { branchLabel, preferredDiffBase } from "../../lib/branch";
import { buildDiffTree, type DiffNode } from "../../lib/difftree";
import {
  execDir,
  type GitBranch,
  type GitDiffHunk,
  type GitDiffLine,
  type GitDiffResult,
} from "../../lib/types";
import { cn } from "../../lib/utils";

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function linePrefix(kind: GitDiffLine["kind"]): string {
  if (kind === "add") return "+";
  if (kind === "remove") return "-";
  return " ";
}

function lineNumbers(hunk: GitDiffHunk): { old: number | null; new: number | null }[] {
  const parts = hunk.header.split(/\s+/);
  const parse = (value: string | undefined, sign: "+" | "-"): number => {
    const raw = value?.startsWith(sign) ? value.slice(1) : "";
    return Number(raw.split(",", 1)[0]) || 1;
  };
  let old = parse(parts[1], "-");
  let next = parse(parts[2], "+");
  return hunk.lines.map((line) => {
    if (line.kind === "add") {
      const numbers = { old: null, new: next };
      next += 1;
      return numbers;
    }
    if (line.kind === "remove") {
      const numbers = { old, new: null };
      old += 1;
      return numbers;
    }
    const numbers = { old, new: next };
    old += 1;
    next += 1;
    return numbers;
  });
}

type DiffCommentTarget =
  | { kind: "file"; path: string }
  | {
      kind: "code";
      path: string;
      hunkIndex: number;
      endLine: number;
      code: string;
    };

type DiffLineRef = { path: string; hunkIndex: number; lineIndex: number };
type DiffDrag = { start: DiffLineRef; end: DiffLineRef };

function sameHunk(a: DiffLineRef, b: DiffLineRef): boolean {
  return a.path === b.path && a.hunkIndex === b.hunkIndex;
}

function lineRefFromElement(element: Element | null): DiffLineRef | null {
  const line = element?.closest<HTMLElement>("[data-diff-line]");
  if (line == null) return null;
  const path = line.dataset.diffFile;
  const hunkIndex = Number(line.dataset.diffHunk);
  const lineIndex = Number(line.dataset.diffLineIndex);
  if (path === undefined || Number.isNaN(hunkIndex) || Number.isNaN(lineIndex)) return null;
  return { path, hunkIndex, lineIndex };
}

function lineRefAtPoint(x: number, y: number): DiffLineRef | null {
  return lineRefFromElement(document.elementFromPoint(x, y));
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
  const [branches, setBranches] = useState<GitBranch[]>([]);
  const [base, setBase] = useState<string | null>(null);
  const [basePickerOpen, setBasePickerOpen] = useState(false);
  const [baseQuery, setBaseQuery] = useState("");
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
  const [hoveredGutter, setHoveredGutter] = useState<string | null>(null);
  const contentRef = useRef<HTMLDivElement | null>(null);
  /** 悬浮横向滚动条是否按下（用原生事件同步，避免拖动时 React 重渲染卡顿） */
  const hScrollDragRef = useRef(false);
  /** 悬浮横向滚动条槽按下位置与按下时的 scrollLeft，用于 pointer capture 拖动 */
  const hScrollTrackRef = useRef<{ pointerId: number; x: number; scrollLeft: number } | null>(null);
  const dragRef = useRef<DiffDrag | null>(null);
  const dragPointerRef = useRef<number | null>(null);
  const diffRequestRef = useRef(0);
  const basePickerRef = useRef<HTMLDivElement | null>(null);
  const useWorktree = (state.detail.session?.worktreeDir ?? "") !== "";
  const branchMachine = state.detail.session?.machine ?? null;
  const branchRepo = state.detail.session ? execDir(state.detail.session) : null;

  // 打开时刷新一次（docs/DESIGN.md「改动审查视图」）：会话切换时重新拉取
  useEffect(() => {
    setDiff(null);
    setBranches([]);
    setBase(null);
    setBasePickerOpen(false);
    setBaseQuery("");
    setError(null);
    setDiffsCollapsed(false);
    setCollapsedDirs([]);
    setSelected(null);
    setCommentTarget(null);
    setCommentText("");
    setCommentSending(false);
    setDragRange(null);
    setHoveredGutter(null);
    dragRef.current = null;
    dragPointerRef.current = null;
    hScrollDragRef.current = false;
    hScrollTrackRef.current = null;
    const request = ++diffRequestRef.current;
    const client = core.client;
    if (client === null || sessionId === null || branchMachine === null || branchRepo === null) return;
    let cancelled = false;
    void (async () => {
      const branchResult = await client
        .listBranches(branchMachine, branchRepo)
        .catch(() => ({ branches: [] }));
      if (cancelled || request !== diffRequestRef.current) return;
      const selected = preferredDiffBase(branchResult.branches, useWorktree);
      setBranches(branchResult.branches);
      setBase(selected);
      if (selected === null) return;
      try {
        const result = await client.diff(sessionId, selected);
        if (!cancelled && request === diffRequestRef.current) setDiff(result);
      } catch (cause) {
        if (!cancelled && request === diffRequestRef.current) setError(messageOf(cause));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [branchMachine, branchRepo, core, sessionId, useWorktree]);

  useEffect(() => {
    if (!basePickerOpen) return;
    const onMouseDown = (event: MouseEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (basePickerRef.current !== null && !basePickerRef.current.contains(target)) {
        setBasePickerOpen(false);
        setBaseQuery("");
      }
    };
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
  }, [basePickerOpen]);

  const files = diff?.files ?? [];
  const nodes = useMemo(() => buildDiffTree(diff?.files ?? []), [diff]);
  const additions = files.reduce((total, file) => total + file.additions, 0);
  const deletions = files.reduce((total, file) => total + file.deletions, 0);

  const refreshDiff = () => {
    if (sessionId === null || core.client === null || base === null) return;
    const request = ++diffRequestRef.current;
    setError(null);
    setDiff(null);
    core.client.diff(sessionId, base).then(
      (result) => {
        if (request === diffRequestRef.current) setDiff(result);
      },
      (cause: unknown) => {
        if (request === diffRequestRef.current) setError(messageOf(cause));
      },
    );
  };

  const selectBase = (value: string) => {
    setBasePickerOpen(false);
    setBaseQuery("");
    if (sessionId === null || value === base) return;
    setBase(value);
    setDiff(null);
    setError(null);
    const request = ++diffRequestRef.current;
    const client = core.client;
    if (client === null) return;
    client.diff(sessionId, value).then(
      (result) => {
        if (request === diffRequestRef.current) setDiff(result);
      },
      (cause: unknown) => {
        if (request === diffRequestRef.current) setError(messageOf(cause));
      },
    );
  };

  const baseKeyword = baseQuery.trim().toLowerCase();
  const matchedBranches =
    baseKeyword === ""
      ? branches
      : branches.filter(
          (branch) =>
            branch.name.toLowerCase().includes(baseKeyword) ||
            branch.fullName.toLowerCase().includes(baseKeyword),
        );
  const selectedBranch = branches.find((branch) => branch.fullName === base);
  const baseLabel =
    selectedBranch === undefined ? (base ?? "未选择") : branchLabel(selectedBranch);

  const toggleDir = (key: string) => {
    setCollapsedDirs((prev) =>
      prev.includes(key) ? prev.filter((item) => item !== key) : [...prev, key],
    );
  };

  const selectFile = (index: number) => {
    setSelected(index);
    const content = contentRef.current;
    if (content === null) return;
    const file = content.querySelector(`#diff-file-${index}`);
    if (file === null) return;
    // 手动换算滚动量：文件顶部贴住内容区顶部，不滚动横向滚动条
    content.scrollTop = file.getBoundingClientRect().top - content.getBoundingClientRect().top + content.scrollTop;
  };

  const syncFloatingHScroll = () => {
    const content = contentRef.current;
    const track = document.querySelector<HTMLElement>("[data-slot='diff-hscroll-track']");
    const thumb = track?.querySelector<HTMLElement>("[data-slot='diff-hscroll-thumb']");
    if (content === null || track === null || thumb == null) return;
    const client = content.clientWidth;
    const scroll = content.scrollWidth;
    if (scroll <= client) {
      track.style.display = "none";
      return;
    }
    track.style.display = "";
    const thumbWidth = Math.max((client / scroll) * client, 24);
    const maxLeft = client - thumbWidth;
    thumb.style.width = `${thumbWidth}px`;
    thumb.style.transform = `translateX(${(content.scrollLeft / (scroll - client)) * maxLeft}px)`;
  };

  const beginHScroll = (event: ReactPointerEvent<HTMLDivElement>) => {
    const content = contentRef.current;
    const thumb = event.currentTarget.querySelector<HTMLElement>("[data-slot='diff-hscroll-thumb']");
    if (content === null || thumb === null) return;
    hScrollDragRef.current = true;
    hScrollTrackRef.current = {
      pointerId: event.pointerId,
      x: event.clientX,
      scrollLeft: content.scrollLeft,
    };
    event.currentTarget.setPointerCapture(event.pointerId);
    // 点到滑槽空白处：按点击位置比例跳转滚动位置
    if (!thumb.contains(event.target as Node)) {
      const client = content.clientWidth;
      const scroll = content.scrollWidth;
      const thumbWidth = thumb.getBoundingClientRect().width;
      const rect = event.currentTarget.getBoundingClientRect();
      const maxLeft = client - thumbWidth;
      const ratio = (event.clientX - rect.left - thumbWidth / 2) / maxLeft;
      content.scrollLeft = Math.min(Math.max(ratio, 0), 1) * (scroll - client);
    }
  };

  const moveHScroll = (event: ReactPointerEvent<HTMLDivElement>) => {
    const track = hScrollTrackRef.current;
    const content = contentRef.current;
    if (track === null || content === null || track.pointerId !== event.pointerId) return;
    const client = content.clientWidth;
    const scroll = content.scrollWidth;
    const thumbWidth = Math.max((client / scroll) * client, 24);
    const maxLeft = client - thumbWidth;
    content.scrollLeft =
      track.scrollLeft + ((event.clientX - track.x) / maxLeft) * (scroll - client);
  };

  const endHScroll = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (hScrollTrackRef.current?.pointerId !== event.pointerId) return;
    hScrollTrackRef.current = null;
    hScrollDragRef.current = false;
  };

  const openFileComment = (path: string) => {
    setCommentTarget({ kind: "file", path });
    setCommentText("");
    setDragRange(null);
    dragRef.current = null;
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
      setDragRange(null);
    }
  };

  const cancelComment = () => {
    setCommentTarget(null);
    setCommentText("");
    setDragRange(null);
    dragRef.current = null;
  };

  const beginCodeSelection = (
    event: ReactPointerEvent<HTMLSpanElement>,
    line: DiffLineRef,
  ) => {
    if (event.button !== 0) return;
    event.preventDefault();
    event.stopPropagation();
    const drag = { start: line, end: line };
    dragRef.current = drag;
    dragPointerRef.current = event.pointerId;
    setDragRange(drag);
  };

  const extendCodeSelectionAtEvent = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.pointerId !== dragPointerRef.current) return;
    const direct =
      event.target instanceof Element ? lineRefFromElement(event.target) : null;
    const next = direct ?? lineRefAtPoint(event.clientX, event.clientY);
    if (next !== null) extendCodeSelection(next);
  };

  const finishCodeSelectionAtEvent = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.pointerId !== dragPointerRef.current) return;
    const direct =
      event.target instanceof Element ? lineRefFromElement(event.target) : null;
    const next = direct ?? lineRefAtPoint(event.clientX, event.clientY);
    if (next !== null) extendCodeSelection(next);
    const current = dragRef.current;
    dragPointerRef.current = null;
    if (current !== null) finishCodeSelection(current);
  };

  const cancelCodeSelection = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.pointerId !== dragPointerRef.current) return;
    dragPointerRef.current = null;
    cancelComment();
  };

  const extendCodeSelection = (line: DiffLineRef) => {
    const current = dragRef.current;
    if (current === null || !sameHunk(current.start, line)) return;
    const drag = { ...current, end: line };
    dragRef.current = drag;
    setDragRange(drag);
  };

  const finishCodeSelection = (drag: DiffDrag) => {
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
    setCommentTarget({
      kind: "code",
      path: drag.start.path,
      hunkIndex: drag.start.hunkIndex,
      endLine: end,
      code,
    });
    setCommentText("");
  };

  let body: ReactNode;
  if (error !== null) {
    body = <Hint text={`读取改动失败：${error}`} />;
  } else if (base === null) {
    body = <Hint text="请选择基准分支" />;
  } else if (diff === null) {
    body = <Hint text="正在加载改动…" />;
  } else if (diff.notRepo === true) {
    body = <Hint text="当前执行目录不是 git 仓库" />;
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
            onPointerMove={extendCodeSelectionAtEvent}
            onPointerUp={finishCodeSelectionAtEvent}
            onPointerCancel={cancelCodeSelection}
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
                  <span className="shrink-0 text-success">+{file.additions}</span>
                  <span className="shrink-0 text-destructive">-{file.deletions}</span>
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
                {!diffsCollapsed ? (
                  <div data-slot="diff-file-scroll" className="overflow-x-auto">
                    {file.hunks.map((hunk, hunkIx) => {
                      const numbers = lineNumbers(hunk);
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
                          <div className="flex w-max min-w-full px-2 font-mono text-xs text-muted-foreground">
                            <span className="min-w-0 flex-1">{hunk.header}</span>
                          </div>
                          {hunk.lines.map((line, lineIx) => {
                            const lineNumber = numbers[lineIx];
                            const gutterKey = `${file.path}:${hunkIx}:${lineIx}`;
                            return (
                              <Fragment key={lineIx}>
                                <div
                                  data-slot="diff-line"
                                  data-diff-file={file.path}
                                  data-diff-hunk={hunkIx}
                                  data-diff-line-index={lineIx}
                                  onPointerEnter={() =>
                                    extendCodeSelection({
                                      path: file.path,
                                      hunkIndex: hunkIx,
                                      lineIndex: lineIx,
                                    })
                                  }
                                  className={cn(
                                    "flex w-max min-w-full whitespace-pre font-mono text-xs",
                                    line.kind === "add" && "bg-diff-add",
                                    line.kind === "remove" && "bg-diff-remove",
                                    selectedLines !== null &&
                                      lineIx >= selectedLines[0] &&
                                      lineIx <= selectedLines[1] &&
                                      "ring-1 ring-inset ring-primary/60",
                                  )}
                                >
                                  <span
                                    data-diff-gutter="true"
                                    className="relative flex w-6 shrink-0 cursor-pointer touch-none items-center justify-center text-muted-foreground select-none"
                                    onPointerDown={(event) =>
                                      beginCodeSelection(event, {
                                        path: file.path,
                                        hunkIndex: hunkIx,
                                        lineIndex: lineIx,
                                      })
                                    }
                                    onPointerEnter={() => setHoveredGutter(gutterKey)}
                                    onPointerLeave={() =>
                                      setHoveredGutter((current) =>
                                        current === gutterKey ? null : current,
                                      )
                                    }
                                  >
                                    {hoveredGutter === gutterKey ? (
                                      <Plus className="size-3 text-primary" />
                                    ) : (
                                      (lineNumber.old ?? "")
                                    )}
                                  </span>
                                  <span className="w-9 shrink-0 px-2 text-right text-muted-foreground select-none">
                                    {lineNumber.new ?? ""}
                                  </span>
                                  <span
                                    className={cn(
                                      "w-4 shrink-0 text-center font-semibold",
                                      line.kind === "add" && "text-success",
                                      line.kind === "remove" && "text-destructive",
                                    )}
                                  >
                                    {linePrefix(line.kind)}
                                  </span>
                                  <span>{line.text}</span>
                                </div>
                                {commentTarget?.kind === "code" &&
                                commentTarget.path === file.path &&
                                commentTarget.hunkIndex === hunkIx &&
                                commentTarget.endLine === lineIx ? (
                                  <CommentComposer
                                    target={commentTarget}
                                    value={commentText}
                                    sending={commentSending}
                                    onChange={setCommentText}
                                    onCancel={cancelComment}
                                    onSubmit={() => void submitComment()}
                                  />
                                ) : null}
                              </Fragment>
                            );
                          })}
                        </div>
                      );
                    })}
                  </div>
                ) : null}
              </div>
            ))}
          </div>
        }
        treeSlot="diff-tree"
        contentSlot="diff-files"
        treeClassName="max-h-[40%] min-h-0 overflow-y-auto rounded-md bg-muted/40 p-1 lg:h-full lg:max-h-none"
        contentClassName="min-h-0 flex-1 overflow-y-auto rounded-md bg-muted/20 lg:h-full"
        contentRef={(node) => {
          contentRef.current = node;
          if (node === null) return;
          // 悬浮横向滚动条用原生监听同步，避开 React 滚动重渲染
          node.addEventListener("scroll", syncFloatingHScroll);
          syncFloatingHScroll();
        }}
        contentCollapsed={diffsCollapsed}
        contentFooter={
          // 悬浮横向滚动条：固定在内容区可视底部，内容超长时无需滚到底即可左右滚动
          <div
            data-slot="diff-hscroll-track"
            role="scrollbar"
            aria-orientation="horizontal"
            className="absolute right-0 bottom-1 left-0 z-10 mx-3 hidden h-3 cursor-pointer touch-none rounded-full bg-background/80 lg:flex lg:items-center"
            onPointerDown={beginHScroll}
            onPointerMove={moveHScroll}
            onPointerUp={endHScroll}
            onPointerCancel={endHScroll}
          >
            <div
              data-slot="diff-hscroll-thumb"
              className="h-1.5 rounded-full bg-border"
            />
          </div>
        }
      />
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      {/* 工具栏（docs/PRD.md「改动审查视图」）：折叠/展开文件树按钮左对齐，
          折叠/展开 diff 区域按钮右对齐（折叠后仅显示文件名） */}
      <div className="grid shrink-0 grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] items-center gap-2 px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <Button
            type="button"
            variant="ghost"
            size="icon"
            data-slot="diff-toggle-tree"
            aria-label={treeVisible ? "折叠文件树" : "展开文件树"}
            title={treeVisible ? "折叠文件树" : "展开文件树"}
            onClick={() => setTreeVisible((prev) => !prev)}
          >
            {treeVisible ? <PanelLeftClose /> : <PanelLeftOpen />}
          </Button>
          <span data-slot="diff-file-count" className="text-xs text-muted-foreground">
            {files.length} 个文件
          </span>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            data-slot="diff-refresh"
            aria-label="刷新改动"
            title="刷新改动"
            onClick={refreshDiff}
          >
            <RefreshCw />
          </Button>
        </div>
        <div ref={basePickerRef} className="relative w-40 lg:w-56">
          <button
            type="button"
            role="combobox"
            aria-label="选择基准分支"
            aria-expanded={basePickerOpen}
            data-slot="diff-base-select"
            onClick={() => {
              setBasePickerOpen((open) => !open);
              setBaseQuery("");
            }}
            className={cn(
              "flex h-8 w-full items-center gap-2 rounded-md border border-input bg-background px-2 text-sm outline-none transition-colors hover:bg-accent focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-ring/40",
              basePickerOpen && "border-ring ring-2 ring-ring/40",
            )}
          >
            <span className="min-w-0 flex-1 truncate text-left">{baseLabel}</span>
            <ChevronDown className="size-4 shrink-0 text-muted-foreground" />
          </button>
          {basePickerOpen ? (
            <div
              role="listbox"
              data-slot="diff-base-list"
              className="absolute top-full left-1/2 z-20 mt-1 flex max-h-64 w-64 -translate-x-1/2 flex-col overflow-y-auto rounded-md border border-border bg-popover p-1 shadow-lg"
            >
              <Input
                autoFocus
                data-slot="diff-base-search"
                aria-label="搜索基准分支"
                placeholder="搜索分支"
                className="mb-1 h-8"
                value={baseQuery}
                onKeyDown={(event) => {
                  if (event.key === "Escape") {
                    setBasePickerOpen(false);
                    setBaseQuery("");
                  }
                }}
                onChange={(event) => setBaseQuery(event.target.value)}
              />
              {matchedBranches.length === 0 ? (
                <span className="px-2 py-1.5 text-xs text-muted-foreground">无匹配分支</span>
              ) : (
                matchedBranches.map((branch) => (
                  <button
                    key={branch.fullName}
                    type="button"
                    role="option"
                    aria-selected={base === branch.fullName}
                    className="flex cursor-pointer items-center rounded-sm px-2 py-1.5 text-left text-sm hover:bg-accent"
                    onClick={() => selectBase(branch.fullName)}
                  >
                    <span className="min-w-0 flex-1 truncate">{branchLabel(branch)}</span>
                  </button>
                ))
              )}
            </div>
          ) : null}
        </div>
        <div className="flex min-w-0 items-center justify-end gap-2">
          <span data-slot="diff-line-count" className="text-xs">
            <span className="text-success">+{additions}</span>
            <span className="text-muted-foreground">/</span>
            <span className="text-destructive">-{deletions}</span>
          </span>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            data-slot="diff-toggle-changes"
            aria-label={diffsCollapsed ? "展开 diff 区域" : "折叠 diff 区域"}
            title={diffsCollapsed ? "展开 diff 区域" : "折叠 diff 区域"}
            onClick={() => setDiffsCollapsed((prev) => !prev)}
          >
            {diffsCollapsed ? <PanelRightOpen /> : <PanelRightClose />}
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
