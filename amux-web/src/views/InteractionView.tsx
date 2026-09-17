// 会话交互视图（docs/PRD.md「会话交互视图」）：agent 状态、对话气泡、实时活动、
// 快捷指令栏、输入区（Enter 发送 / Shift+Enter 换行、斜杠命令上拉框、附件）、会话选项。

import { useEffect, useMemo, useRef, useState } from "react";
import { SendHorizontal, Square } from "lucide-react";

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
import { loadOlderHistory } from "../core/poll";
import { useCore, useCoreState } from "../core/store";
import { activitySummary, formatTime, truncate } from "../lib/format";
import { pageSizeForViewport } from "../lib/paging";
import { matchSlashCommands } from "../lib/slash";
import type { ContentBlock, HistoryItem, SessionConfigOption } from "../lib/types";
import { cn } from "../lib/utils";

export function InteractionView() {
  const core = useCore();
  const state = useCoreState();
  const target = state.open;
  const detail = state.detail;

  const [draft, setDraft] = useState("");
  const listRef = useRef<HTMLDivElement>(null);
  const heightBeforeLoad = useRef<number | null>(null);

  useEffect(() => {
    setDraft("");
  }, [target?.kind, target?.id]);

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
    : "编排智能体";
  const available = isSession
    ? (state.settings.agents
        .find((entry) => entry.machine === detail.session?.machine)
        ?.agents.find((agent) => agent.name === detail.session?.agent)?.available ?? false)
    : state.settings.orchestrator.status === "ready" && state.settings.orchestrator.config !== null;

  const onScroll = () => {
    const element = listRef.current;
    if (!element) return;
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
    if (
      element.scrollTop <= 0 &&
      detail.historyPaging.hasOlder &&
      !detail.historyPaging.loadingOlder
    ) {
      heightBeforeLoad.current = element.scrollHeight;
      void loadOlderHistory(core);
    }
  };

  const send = async () => {
    if (draft.trim() === "" && state.attachments.length === 0) return;
    const ok = await sendPrompt(core, draft);
    if (ok) setDraft("");
  };

  const quickSend = async (prompt: string) => {
    await sendPrompt(core, prompt);
  };

  return (
    <div data-slot="interaction-view" className="flex h-full min-h-0 flex-col">
      <header className="flex items-center gap-2 border-b border-border py-2 pr-12 pl-3">
        <span data-slot="interaction-agent" className="font-medium">
          {title}
        </span>
        <span
          data-slot="interaction-agent-state"
          data-available={available}
          className={cn("text-xs", available ? "text-muted-foreground" : "text-destructive")}
        >
          {available ? "可用" : "不可用"}
        </span>
        {detail.workflow ? (
          <span className="ml-auto text-xs text-muted-foreground">
            {truncate(detail.workflow.title || "未命名会话", 24)}
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
          {truncate(activitySummary(detail.ongoing), 200)}
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
                className="flex w-full flex-col items-start px-2 py-1 text-left hover:bg-accent"
                onClick={() => setDraft(`/${command.name} `)}
              >
                <span>/{command.name}</span>
                <span className="text-xs text-muted-foreground">{command.description}</span>
              </button>
            ))}
          </div>
        ) : null}

        <div className="flex items-end gap-3">
          <div className="min-w-0 flex-1">
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
                      onClick={() => removeAttachment(core, index)}
                    >
                      ×
                    </button>
                  </span>
                ))}
              </div>
            ) : null}
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
                data-slot="prompt-input"
                aria-label="消息输入框"
                rows={3}
                placeholder="输入指令，Enter 发送，Shift+Enter 换行"
                value={draft}
                onChange={(event) => setDraft(event.target.value)}
                onPaste={(event) => {
                  const files = [...event.clipboardData.files];
                  if (files.length > 0) {
                    event.preventDefault();
                    void addFiles(core, files);
                  }
                }}
                onKeyDown={(event) => {
                  if (event.key !== "Enter" || event.shiftKey) return;
                  event.preventDefault();
                  void send();
                }}
              />
            </div>
          </div>
          <div className="flex shrink-0 flex-col gap-2">
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
          <SelectTrigger data-slot="option-select" className="h-7 w-40">
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
