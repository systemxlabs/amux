// 会话详情视图：普通会话、工作流会话与群聊会话的元信息（docs/PRD.md「主页面」、
// docs/DESIGN.md「会话详情视图」）。

import { useEffect } from "react";

import { refreshDetails } from "../../core/poll";
import { useCore, useCoreState } from "../../core/store";
import { formatTime, stateLabel } from "../../lib/format";
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
  const group = state.detail.group;
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
  } else if (workflow !== null) {
    rows.push({ label: "会话 ID", value: workflow.id });
    rows.push({ label: "agent", value: "工作流智能体" });
    rows.push({ label: "会话状态", value: stateLabel(workflow.state) });
    rows.push({ label: "标题", value: workflow.title });
    rows.push({ label: "创建时间", value: formatTime(workflow.createdAt) });
    rows.push({ label: "最近活跃", value: formatTime(workflow.updatedAt) });
    rows.push({ label: "工作计划", value: workflow.plan, pre: true });
  } else if (group !== null) {
    rows.push({ label: "会话 ID", value: group.id });
    rows.push({ label: "成员数", value: String(group.members.length) });
    rows.push({ label: "会话状态", value: stateLabel(group.state) });
    rows.push({ label: "标题", value: group.title });
    rows.push({ label: "所属项目", value: group.project ?? "未归属" });
    rows.push({ label: "创建时间", value: formatTime(group.createdAt) });
    rows.push({ label: "最近活跃", value: formatTime(group.updatedAt) });
  }

  return (
    <div data-slot="details-panel" className="flex h-full min-h-0 flex-col">
      <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto p-3">
        {rows.map((row) => (
          <DetailRow key={row.label} label={row.label} value={row.value} pre={row.pre} />
        ))}
        {group !== null ? (
          <div data-slot="group-member-details" className="mt-2 flex flex-col gap-2">
            <div className="text-xs font-medium text-muted-foreground">成员</div>
            {group.members.map((member) => {
              const session = group.sessions.find((item) => item.id === member.sessionId);
              return (
                <div
                  key={member.id}
                  data-slot="group-member-detail"
                  className="rounded-md border border-border p-2 text-sm"
                >
                  <div className="flex flex-wrap items-baseline gap-x-2">
                    <span className="font-medium">{member.name}</span>
                    <span className="text-xs text-muted-foreground">
                      {member.agent}@{member.machine}
                    </span>
                    {member.model ? (
                      <span className="text-xs text-muted-foreground">
                        模型 · {member.model}
                      </span>
                    ) : null}
                    {session !== undefined ? (
                      <span className="text-xs text-muted-foreground">
                        {stateLabel(session.state)}
                      </span>
                    ) : null}
                  </div>
                  <p className="mt-1 whitespace-pre-wrap break-words text-xs text-muted-foreground">
                    {member.description}
                  </p>
                </div>
              );
            })}
          </div>
        ) : null}
      </div>
    </div>
  );
}
