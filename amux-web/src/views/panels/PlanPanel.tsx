// 会话计划视图：agent 计划条目（docs/PRD.md「主页面」、docs/DESIGN.md「计划视图」）。

import { useCoreState } from "../../core/store";
import type { SessionPlanPriority, SessionPlanStatus } from "../../lib/types";
import { cn } from "../../lib/utils";

const STATUS_LABEL: Record<SessionPlanStatus, string> = {
  pending: "待办",
  in_progress: "进行中",
  completed: "已完成",
};

/** 状态色提示：进行中高亮，其余弱化。 */
const STATUS_CLASS: Record<SessionPlanStatus, string> = {
  pending: "text-muted-foreground",
  in_progress: "text-primary",
  completed: "text-muted-foreground",
};

const PRIORITY_LABEL: Record<SessionPlanPriority, string> = {
  high: "高",
  medium: "中",
  low: "低",
};

export function PlanPanel() {
  const state = useCoreState();
  const entries = state.detail.plan;

  return (
    <div data-slot="plan-panel" className="flex h-full min-h-0 flex-col">
      <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto p-3">
        {entries.map((entry, index) => (
          <div key={index} data-slot="plan-entry" className="flex items-start gap-2">
            <span className={cn("shrink-0 text-xs", STATUS_CLASS[entry.status])}>
              {STATUS_LABEL[entry.status]}
            </span>
            <span className="shrink-0 text-xs text-muted-foreground">
              {PRIORITY_LABEL[entry.priority]}
            </span>
            <span
              className={cn(
                "min-w-0 flex-1 text-sm",
                entry.status === "completed" && "text-muted-foreground",
              )}
            >
              {entry.content}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}
