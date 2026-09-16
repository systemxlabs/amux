// 连接状态提示（保存成功/失败等通知，PRD 各处「弹出通知」）。

import { useCoreState } from "../core/store";
import { cn } from "../lib/utils";

export function Notice() {
  const state = useCoreState();
  if (!state.notice) return null;
  return (
    <div
      data-slot="notice"
      data-kind={state.notice.kind}
      className={cn(
        "fixed bottom-4 left-1/2 z-50 -translate-x-1/2 rounded-md border px-3 py-2 shadow-lg",
        state.notice.kind === "success"
          ? "border-border bg-card text-foreground"
          : "border-destructive bg-card text-destructive",
      )}
    >
      {state.notice.text}
    </div>
  );
}
