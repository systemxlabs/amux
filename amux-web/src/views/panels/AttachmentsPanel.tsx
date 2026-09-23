// 会话附件面板（docs/PRD.md「主页面」）：附件列表、删除单项与删除全部。

import { useState } from "react";
import { ArrowDownToLine, Trash2 } from "lucide-react";

import { ConfirmDialog } from "../../components/ConfirmDialog";
import { Button } from "../../components/ui/button";
import {
  deleteAllAttachments,
  deleteAttachment,
  loadMoreAttachments,
} from "../../core/actions";
import { useCore, useCoreState } from "../../core/store";

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function AttachmentsPanel() {
  const core = useCore();
  const state = useCoreState();
  const detail = state.detail;
  const target = state.open;
  const client = core.client;
  const [confirmingDeleteAll, setConfirmingDeleteAll] = useState(false);

  const loadMoreIfNeeded = (element: HTMLDivElement): void => {
    if (
      element.scrollHeight - element.scrollTop - element.clientHeight <= 32 &&
      detail.attachmentsHasMore &&
      !detail.attachmentsLoading
    ) {
      void loadMoreAttachments(core);
    }
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex shrink-0 items-center justify-between border-b border-border px-3 py-2">
        <span className="text-xs text-muted-foreground">
          {detail.attachments.length} 个附件
        </span>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          data-slot="attachments-delete-all"
          disabled={detail.attachments.length === 0}
          onClick={() => setConfirmingDeleteAll(true)}
        >
          <Trash2 />
          删除全部
        </Button>
      </div>
      <div
        data-slot="attachments-list"
        className="min-h-0 flex-1 overflow-y-auto p-3"
        onScroll={(event) => loadMoreIfNeeded(event.currentTarget)}
      >
        {detail.attachmentsLoading && detail.attachments.length === 0 ? (
          <div className="py-4 text-center text-sm text-muted-foreground">加载中…</div>
        ) : detail.attachments.length === 0 ? (
          <div className="py-4 text-center text-sm text-muted-foreground">暂无附件</div>
        ) : (
          <div className="flex flex-col gap-2">
            {detail.attachments.map((attachment) => {
              const uri =
                target === null || client === null
                  ? ""
                  : target.kind === "session"
                    ? client.sessionAttachmentUri(target.id, attachment.name)
                    : client.workflowAttachmentUri(target.id, attachment.name);
              return (
                <div
                  key={attachment.name}
                  data-slot="attachment-item"
                  className="flex items-center gap-2 rounded-md border border-border p-2"
                >
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-sm" title={attachment.name}>
                      {attachment.name}
                    </div>
                    <div className="text-xs text-muted-foreground">
                      {formatSize(attachment.size)}
                    </div>
                  </div>
                  <a
                    href={uri || undefined}
                    target="_blank"
                    rel="noreferrer"
                    aria-label={`下载 ${attachment.name}`}
                    className={
                      uri === ""
                        ? "rounded-md p-2 opacity-50"
                        : "rounded-md p-2 hover:bg-accent"
                    }
                  >
                    <ArrowDownToLine className="size-4" />
                  </a>
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    aria-label={`删除 ${attachment.name}`}
                    onClick={() => void deleteAttachment(core, attachment.name)}
                  >
                    <Trash2 />
                  </Button>
                </div>
              );
            })}
            {detail.attachmentsLoading ? (
              <div className="py-2 text-center text-xs text-muted-foreground">加载中…</div>
            ) : null}
          </div>
        )}
      </div>
      <ConfirmDialog
        open={confirmingDeleteAll}
        title="删除全部附件"
        description="当前会话的全部附件将被删除，此操作不可撤销。"
        confirmLabel="删除"
        onCancel={() => setConfirmingDeleteAll(false)}
        onConfirm={() => {
          setConfirmingDeleteAll(false);
          void deleteAllAttachments(core);
        }}
      />
    </div>
  );
}
