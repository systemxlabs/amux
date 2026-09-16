// 会话详情视图：普通会话与工作流会话的元信息（docs/PRD.md「主页面」、docs/DESIGN.md「会话详情视图」）。

import { useEffect } from "react";

import { refreshDetails } from "../../core/poll";
import { useCore, useCoreState } from "../../core/store";
import { formatContext, formatTime, stateLabel } from "../../lib/format";
import { cn } from "../../lib/utils";

type Row = { label: string; value: string; pre?: boolean };

function DetailRow({ label, value, pre }: Row) {
  return (
    <div data-slot="details-row" className="flex gap-3">
      <div className="w-24 shrink-0 text-xs text-muted-foreground">{label}</div>
      <div className={cn("min-w-0 flex-1 break-words text-sm", pre === true && "whitespace-pre-wrap")}>
        {value}
      </div>
    </div>
  );
}

export function DetailsPanel() {
  const core = useCore();
  const state = useCoreState();
  const targetId = state.open?.id ?? null;

  // 打开时刷新一次，不定时刷新（docs/DESIGN.md「会话详情视图」）
  useEffect(() => {
    if (targetId !== null) void refreshDetails(core);
  }, [core, targetId]);

  const session = state.detail.session;
  const workflow = state.detail.workflow;
  const rows: Row[] = [];
  if (session !== null) {
    rows.push({ label: "会话 ID", value: session.id });
    rows.push({ label: "所属机器", value: session.machine });
    rows.push({ label: "agent", value: session.agent });
    rows.push({ label: "工作目录", value: session.workspace });
    if (session.worktreeDir !== "") {
      rows.push({ label: "worktree 目录", value: session.worktreeDir });
    }
    rows.push({ label: "会话状态", value: stateLabel(session.state) });
    rows.push({ label: "标题", value: session.title });
    rows.push({ label: "创建时间", value: formatTime(session.createdAt) });
    rows.push({ label: "最近活跃", value: formatTime(session.updatedAt) });
    rows.push({
      label: "上下文用量",
      value: formatContext(state.detail.contextSize, state.detail.contextWindowSize),
    });
  } else if (workflow !== null) {
    rows.push({ label: "会话 ID", value: workflow.id });
    rows.push({ label: "agent", value: "编排智能体" });
    rows.push({ label: "会话状态", value: stateLabel(workflow.state) });
    rows.push({ label: "标题", value: workflow.title });
    rows.push({ label: "创建时间", value: formatTime(workflow.createdAt) });
    rows.push({ label: "最近活跃", value: formatTime(workflow.updatedAt) });
    rows.push({ label: "工作计划", value: workflow.plan, pre: true });
  }

  return (
    <div data-slot="details-panel" className="flex h-full min-h-0 flex-col">
      <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto p-3">
        {rows.map((row) => (
          <DetailRow key={row.label} label={row.label} value={row.value} pre={row.pre} />
        ))}
      </div>
    </div>
  );
}
