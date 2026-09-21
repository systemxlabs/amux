// 会话交互视图（docs/PRD.md「会话交互视图」）：agent 状态、对话气泡、实时活动、
// 快捷指令栏、输入区（Enter 发送 / Shift+Enter 换行、斜杠命令上拉框、附件）、会话选项。

import { useEffect, useLayoutEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { Paperclip, SendHorizontal, Square } from "lucide-react";

import { Markdown } from "../components/Markdown";
import { Button } from "../components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../components/ui/select";
import { Switch } from "../components/ui/switch";
import { Textarea } from "../components/ui/textarea";
import { addFiles, cancelOpen, removeAttachment, sendPrompt, setConfigOption } from "../core/actions";
import { loadNewerHistory, loadOlderHistory } from "../core/poll";
import { useCore, useCoreState } from "../core/store";
import { activityBarText, formatTime } from "../lib/format";
import { pageSizeForViewport } from "../lib/paging";
import { matchSlashCommands } from "../lib/slash";
import { rootDir } from "../lib/types";
import type { ContentBlock, HistoryItem, SessionConfigOption } from "../lib/types";
import { cn } from "../lib/utils";
import { useIsMobile } from "../lib/viewport";

/** 输入框拖拽下限：与 Textarea 的 `min-h-16` 一致。 */
const INPUT_MIN_HEIGHT = 64;

export function InteractionView() {
  const core = useCore();
  const state = useCoreState();
  const isMobile = useIsMobile();
  const target = state.open;
  const detail = state.detail;

  const draft = state.inputDraft;
  const listRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const heightBeforeLoad = useRef<number | null>(null);
  const scrollOnEntry = useRef(true);
  const followingBottom = useRef(true);
  const previousTarget = useRef<string | null>(null);
  // 输入框高度（拖拽调整；null 表示用 rows 的默认高度）
  const [inputHeight, setInputHeight] = useState<number | null>(null);
  const [inputResizing, setInputResizing] = useState(false);
  const inputDrag = useRef<{ startY: number; startHeight: number } | null>(null);
  const targetKey = target === null ? null : `${target.kind}:${target.id}`;

  /** 开始拖拽输入框高度（docs/PRD.md「会话交互视图」：多行输入框，可拖拽高度）。 */
  const startInputResize = (event: ReactPointerEvent<HTMLDivElement>): void => {
    const element = inputRef.current;
    if (element === null) return;
    event.preventDefault();
    inputDrag.current = {
      startY: event.clientY,
      startHeight: element.getBoundingClientRect().height,
    };
    setInputResizing(true);
    event.currentTarget.setPointerCapture(event.pointerId);
  };

  /** 向上拖变高：上限取可视区高度，避免把输入区顶出窗口。 */
  const moveInputResize = (event: ReactPointerEvent<HTMLDivElement>): void => {
    const drag = inputDrag.current;
    if (drag === null) return;
    const next = drag.startHeight + (drag.startY - event.clientY);
    setInputHeight(Math.min(Math.max(next, INPUT_MIN_HEIGHT), window.innerHeight));
  };

  const endInputResize = (event: ReactPointerEvent<HTMLDivElement>): void => {
    inputDrag.current = null;
    setInputResizing(false);
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
  };

  // 仅切换会话时重新进入贴底状态；历史轮询更新不能覆盖用户当前阅读位置。
  useLayoutEffect(() => {
    if (previousTarget.current !== targetKey) {
      previousTarget.current = targetKey;
      scrollOnEntry.current = true;
      followingBottom.current = true;
      heightBeforeLoad.current = null;
    }
    const element = listRef.current;
    // 打开时历史异步加载；首次有消息再定位，不能在空列表上消耗此次滚动。
    if (!element || detail.history.length === 0) return;
    if (!scrollOnEntry.current && !followingBottom.current) return;
    element.scrollTop = element.scrollHeight;
    scrollOnEntry.current = false;
  });

  useEffect(() => {
    core.update((next) => {
      next.inputDraft = "";
    });
  }, [core, target?.kind, target?.id]);

  // 更早一页插入后把原首条目保持在原位置（docs/DESIGN.md「对话滚动机制」）
  useEffect(() => {
    const element = listRef.current;
    if (!element) return;
    if (detail.historyPaging.shift === null) return;
    if (heightBeforeLoad.current !== null) {
      element.scrollTop += element.scrollHeight - heightBeforeLoad.current;
      heightBeforeLoad.current = null;
    }
    core.update((next) => {
      next.detail.historyPaging.shift = null;
    });
  }, [core, detail.historyPaging.shift]);

  const slashMatches = useMemo(
    () => matchSlashCommands(detail.slashCommands, draft),
    [detail.slashCommands, draft],
  );

  if (!target) {
    return (
      <div data-slot="interaction-view" className="flex h-full items-center justify-center">
        <p className="text-muted-foreground">请选择或新建会话</p>
      </div>
    );
  }

  const isSession = target.kind === "session";
  const title = isSession
    ? `${detail.session?.agent ?? ""}@${detail.session?.machine ?? ""}`
    : "工作流智能体";
  const available = isSession
    ? (state.settings.agents
        .find((entry) => entry.machine === detail.session?.machine)
        ?.agents.find((agent) => agent.name === detail.session?.agent)?.available ?? false)
    : state.settings.orchestrator.status === "ready" && state.settings.orchestrator.config !== null;
  const workdir = isSession && detail.session ? rootDir(detail.session) : "";

  // 页大小随可视高度自适应：首次渲染与窗口/容器尺寸变化时也重新计算（不只是滚动事件）
  useEffect(() => {
    const element = listRef.current;
    if (!element) return;
    const updatePageSize = () => {
      const size = pageSizeForViewport(
        element.clientHeight,
        element.scrollHeight,
        detail.history.length,
      );
      core.update((next) => {
        next.detail.historyPaging.pageSize = size;
      });
    };
    updatePageSize();
    const observer = new ResizeObserver(updatePageSize);
    observer.observe(element);
    window.addEventListener("resize", updatePageSize);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", updatePageSize);
    };
  }, [core, detail.history.length]);

  const onScroll = () => {
    const element = listRef.current;
    if (!element) return;
    followingBottom.current =
      element.scrollHeight - element.scrollTop - element.clientHeight <= 1;
    const size = pageSizeForViewport(
      element.clientHeight,
      element.scrollHeight,
      detail.history.length,
    );
    if (size !== detail.historyPaging.pageSize) {
      core.update((next) => {
        next.detail.historyPaging.pageSize = size;
      });
    }
    const nearOlderEdge = element.scrollTop <= element.clientHeight;
    const nearNewerEdge =
      element.scrollHeight - element.scrollTop - element.clientHeight <= element.clientHeight;
    if (
      nearOlderEdge &&
      detail.historyPaging.hasOlder &&
      !detail.historyPaging.loadingOlder
    ) {
      heightBeforeLoad.current = element.scrollHeight;
      void loadOlderHistory(core);
    }
    if (
      nearNewerEdge &&
      detail.historyPaging.hasNewer &&
      !detail.historyPaging.loadingNewer
    ) {
      heightBeforeLoad.current = element.scrollHeight;
      void loadNewerHistory(core);
    }
  };

  const send = async () => {
    if (draft.trim() === "" && state.attachments.length === 0) return;
    const ok = await sendPrompt(core, draft);
    if (ok) {
      core.update((next) => {
        next.inputDraft = "";
      });
    }
  };

  const quickSend = async (prompt: string) => {
    await sendPrompt(core, prompt, false);
  };

  return (
    <div data-slot="interaction-view" className="flex h-full min-h-0 flex-col">
      {/* pl-12/pr-12：为左上角的抽屉按钮与右上角的悬浮按钮留出空间 */}
      <header className="flex items-center gap-2 border-b border-border py-2 pr-12 pl-12 lg:pl-3">
        <span data-slot="interaction-agent" className="font-medium">
          {title}
        </span>
        <span
          data-slot="interaction-agent-state"
          data-available={available}
          className={cn(
            "flex items-center gap-1 text-xs",
            available ? "text-muted-foreground" : "text-destructive",
          )}
        >
          <span
            data-slot="interaction-agent-state-dot"
            className={cn("size-1.5 rounded-full", available ? "bg-success" : "bg-destructive")}
          />
          {available ? "可用" : "不可用"}
        </span>
        {isSession && workdir !== "" ? (
          <span
            data-slot="interaction-workdir"
            title={workdir}
            className="min-w-0 max-w-full truncate text-xs text-muted-foreground"
          >
            {workdir}
          </span>
        ) : null}
      </header>

      {/* pr-12：为悬浮按钮留出空间，消息气泡不会被按钮遮挡 */}
      <div
        ref={listRef}
        data-slot="history-list"
        className="min-h-0 flex-1 overflow-y-auto p-3 pr-12"
        onScroll={onScroll}
      >
        {detail.history.map((item) => (
          <MessageBubble key={item.id} item={item} />
        ))}
      </div>

      {detail.ongoing ? (
        <div
          data-slot="live-activity"
          className="truncate border-t border-border py-1 pr-12 pl-3 text-xs text-muted-foreground"
        >
          {activityBarText(detail.ongoing)}
        </div>
      ) : null}

      {state.settings.quickCommands.length > 0 ? (
        <div
          data-slot="quick-command-bar"
          className="flex flex-wrap gap-1 border-t border-border px-3 py-2"
        >
          {state.settings.quickCommands.map((command) => (
            <Button
              key={command.name}
              data-slot="quick-command-button"
              variant="secondary"
              size="sm"
              onClick={() => void quickSend(command.prompt)}
            >
              {command.name}
            </Button>
          ))}
        </div>
      ) : null}

      <div className="relative border-t border-border p-3">
        {slashMatches.length > 0 ? (
          <div
            data-slot="slash-popup"
            className="absolute bottom-full left-3 mb-1 max-h-56 w-72 overflow-y-auto rounded-md border border-border bg-popover py-1 shadow-lg"
          >
            {slashMatches.map((command) => (
              <button
                key={command.name}
                type="button"
                data-slot="slash-command"
                className="flex w-full flex-col items-start px-2 py-2 text-left hover:bg-accent lg:py-1"

                onClick={() =>
                  core.update((next) => {
                    next.inputDraft = `/${command.name} `;
                  })
                }
              >
                <span>/{command.name}</span>
                <span className="text-xs text-muted-foreground">{command.description}</span>
              </button>
            ))}
          </div>
        ) : null}

        {state.attachments.length > 0 ? (
          <div className="mb-2 flex flex-wrap gap-1">
            {state.attachments.map((attachment, index) => (
              <span
                key={`${attachment.label}-${index}`}
                data-slot="attachment-chip"
                className="flex items-center gap-1 rounded-sm bg-muted px-2 py-0.5 text-xs"
              >
                {attachment.label}
                <button
                  type="button"
                  aria-label="移除附件"
                  className="-my-1 px-1 py-1 text-sm leading-none lg:my-0 lg:text-xs"
                  onClick={() => removeAttachment(core, index)}
                >
                  ×
                </button>
              </span>
            ))}
          </div>
        ) : null}

        {/* 高度拖拽手柄：外层保留触控命中区，视觉上仅显示输入框上沿的细线 */}
        <div
          data-slot="prompt-resize-handle"
          role="separator"
          aria-label="拖拽调整输入框高度"
          aria-orientation="horizontal"
          className={cn(
            "group flex h-3 w-full touch-none cursor-row-resize items-center",
            inputResizing && "bg-primary/10",
          )}
          onPointerDown={startInputResize}
          onPointerMove={moveInputResize}
          onPointerUp={endInputResize}
          onPointerCancel={endInputResize}
        >
          <div
            className={cn(
              "h-px w-full rounded-full transition-colors",
              inputResizing ? "bg-primary" : "bg-border/60 group-hover:bg-primary/70",
            )}
          />
        </div>

        <div className="rounded-md border border-input bg-background focus-within:border-ring focus-within:ring-2 focus-within:ring-ring/40">
          <div
            onDragOver={(event) => event.preventDefault()}
            onDrop={(event) => {
              event.preventDefault();
              if (event.dataTransfer.files.length > 0) {
                void addFiles(core, event.dataTransfer.files);
              }
            }}
          >
            <Textarea
              ref={inputRef}
              data-slot="prompt-input"
              aria-label="消息输入框"
              rows={3}
              className="min-h-16 resize-none rounded-none border-0 bg-transparent px-2.5 pt-2 pb-1 focus-visible:border-0 focus-visible:ring-0"
              style={inputHeight === null ? undefined : { height: inputHeight }}
              placeholder="输入指令，Enter 发送，Shift+Enter 换行"
              value={draft}
              onChange={(event) =>
                core.update((next) => {
                  next.inputDraft = event.target.value;
                })
              }
              onPaste={(event) => {
                const files = [...event.clipboardData.files];
                if (files.length > 0) {
                  event.preventDefault();
                  void addFiles(core, files);
                }
              }}
              onKeyDown={(event) => {
                if (event.key !== "Enter" || event.shiftKey) return;
                // 输入法组字中的回车是候选确认，不是发送
                if (event.nativeEvent.isComposing) return;
                // 软键盘没有 Shift，回车用于换行，发送交给发送按钮
                if (isMobile) return;
                event.preventDefault();
                void send();
              }}
            />
          </div>
          <div className="flex items-center justify-between gap-2 px-2 pb-2">
            <input
              ref={fileInputRef}
              data-slot="attachment-input"
              type="file"
              multiple
              className="hidden"
              onChange={(event) => {
                if (event.target.files !== null) {
                  void addFiles(core, event.target.files);
                }
                event.target.value = "";
              }}
            />
            <Button
              data-slot="attachment-button"
              variant="ghost"
              size="sm"
              onClick={() => fileInputRef.current?.click()}
            >
              <Paperclip className="size-3" />
              附件
            </Button>
            <div className="flex items-center gap-2">
              <Button
                data-slot="cancel-button"
                variant="outline"
                size="sm"
                onClick={() => void cancelOpen(core)}
              >
                <Square className="size-3" />
                取消
              </Button>
              <Button data-slot="send-button" size="sm" onClick={() => void send()}>
                <SendHorizontal className="size-3" />
                发送
              </Button>
            </div>
          </div>
        </div>

        {isSession && detail.configOptions.length > 0 ? (
          <div data-slot="session-options" className="mt-3 flex flex-wrap gap-3">
            {detail.configOptions.map((option) => (
              <SessionOption key={option.id} option={option} />
            ))}
          </div>
        ) : null}
      </div>
    </div>
  );
}

function MessageBubble({ item }: { item: HistoryItem }) {
  const mine = item.role === "user";
  return (
    <div
      data-slot="message"
      data-role={item.role}
      className={cn("mb-3 flex flex-col", mine ? "items-end" : "items-start")}
    >
      <div
        data-slot="message-content"
        className={cn(
          "flex max-w-[80%] flex-col gap-1.5 rounded-md px-3 py-2",
          mine ? "bg-primary text-primary-foreground" : "bg-muted",
        )}
      >
        <MessageContent content={item.content} />
      </div>
      <span data-slot="message-time" className="mt-1 text-xs text-muted-foreground">
        {formatTime(item.timestamp)}
      </span>
    </div>
  );
}

/** 消息气泡内容：文本块直接展示，资源块展示文本内容与图片，URI 资源提供打开入口。 */
function MessageContent({ content }: { content: readonly ContentBlock[] }) {
  return (
    <>
      {content.map((block, index) => (
        <BlockContent key={index} block={block} />
      ))}
    </>
  );
}

function BlockContent({ block }: { block: ContentBlock }) {
  switch (block.type) {
    case "text":
      return <Markdown text={block.text} />;
    case "resource":
      if (block.blob && block.mimeType.startsWith("image/")) {
        return (
          <div className="flex flex-col gap-1">
            {block.uri ? (
              <span data-slot="message-resource-name" className="text-xs opacity-80">
                {block.uri}
              </span>
            ) : null}
            <img
              data-slot="message-resource-image"
              alt={block.uri ?? "图片附件"}
              src={`data:${block.mimeType};base64,${block.blob}`}
              className="max-h-60 w-full rounded object-contain"
            />
          </div>
        );
      }
      if (block.text) {
        return (
          <div className="flex flex-col gap-1">
            {block.uri ? (
              <span data-slot="message-resource-name" className="text-xs opacity-80">
                {block.uri}
              </span>
            ) : null}
            <p data-slot="message-resource-text" className="whitespace-pre-wrap">
              {block.text}
            </p>
          </div>
        );
      }
      if (block.uri) {
        return (
          <a
            data-slot="message-resource-link"
            href={block.uri}
            className="whitespace-pre-wrap underline"
          >
            [资源 {block.uri}]
          </a>
        );
      }
      return <span>[资源]</span>;
    case "resource_link":
      return (
        <a data-slot="message-resource-link" href={block.uri} className="whitespace-pre-wrap underline">
          [引用 {block.title ?? block.name}]
        </a>
      );
  }
}

/** 会话选项：下拉或开关（PRD「会话选项」）。 */
function SessionOption({ option }: { option: SessionConfigOption }) {
  const core = useCore();
  return (
    <label className="flex items-center gap-2 text-xs" data-slot="session-option">
      <span className="text-muted-foreground">{option.name}</span>
      {option.type === "select" ? (
        <Select
          value={option.current_value}
          onValueChange={(value) => void setConfigOption(core, option.id, { type: "value_id", value })}
        >
          {/* 宽度随当前值自适应（组件基类是 w-fit），超长时按上限省略为省略号（见 components/ui/select.tsx） */}
          <SelectTrigger data-slot="option-select" className="h-10 max-w-40 lg:h-7 lg:max-w-56">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {option.options.map((entry) => (
              <SelectItem key={entry.value} value={entry.value}>
                {entry.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      ) : (
        <Switch
          data-slot="option-switch"
          checked={option.current_value}
          onCheckedChange={(checked) =>
            void setConfigOption(core, option.id, { type: "boolean", value: checked })
          }
        />
      )}
    </label>
  );
}
