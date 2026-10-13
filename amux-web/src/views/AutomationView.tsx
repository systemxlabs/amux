// 自动化列表、编辑与详情视图（docs/PRD.md「自动化」、docs/DESIGN.md「Client-Server 通信」）。

import { useEffect, useRef, useState, type ReactNode } from "react";
import {
  Clock3,
  Github,
  Pause,
  Pencil,
  Play,
  Plus,
  Trash2,
  X,
} from "lucide-react";

import { ConfirmDialog } from "../components/ConfirmDialog";
import { ContextMenu, type MenuItem, type MenuState } from "../components/ContextMenu";
import { MultiSelectCombobox } from "../components/MultiSelectCombobox";
import {
  WorkflowPlanField,
  WorkspaceField,
  WorktreeBranchField,
} from "../components/SessionCreationFields";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { Label } from "../components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../components/ui/select";
import { Switch } from "../components/ui/switch";
import { Tabs, TabsList, TabsTrigger } from "../components/ui/tabs";
import { Textarea } from "../components/ui/textarea";
import {
  closeAutomationPanel,
  deleteAutomation,
  openEntry,
  saveAutomation,
  showAutomationDetail,
  showNewAutomation,
  toggleAutomation,
  triggerAutomation,
} from "../core/actions";
import {
  loadMoreAutomationSessions,
  loadMoreAutomationWorkflows,
  resetAutomationTargets,
  refreshAutomations,
  refreshNewSession,
} from "../core/poll";
import { useCore, useCoreState } from "../core/store";
import { previewCron } from "../lib/cron";
import { formatDuration, formatTime } from "../lib/format";
import type {
  Automation,
  AutomationInput,
  AutomationResultLabel,
  AutomationRun,
  AutomationSessionOptions,
  AutomationTrigger,
} from "../lib/types";
import { cn } from "../lib/utils";

type EditorSessionMode =
  | "existing_session"
  | "existing_workflow"
  | "new_session"
  | "new_workflow";

const GITHUB_WEBHOOK_EVENTS = [
  "push",
  "pull_request",
  "issues",
  "issue_comment",
  "pull_request_review",
  "pull_request_review_comment",
  "release",
  "workflow_run",
] as const;

const AUTOMATION_RUNS_PAGE_SIZE = 20;
const RUNS_SCROLL_THRESHOLD = 32;

function triggerSummary(automation: Automation): string {
  if (automation.trigger.type === "schedule") {
    return automation.nextRunAt === undefined
      ? `定时 · ${automation.trigger.cron}`
      : `定时 · 下次 ${formatTime(automation.nextRunAt)}`;
  }
  const events =
    automation.trigger.events === undefined || automation.trigger.events.length === 0
      ? "全部事件"
      : automation.trigger.events.join("、");
  return `GitHub · ${automation.trigger.repository} · ${events}`;
}

function sessionOptionsSummary(options: AutomationSessionOptions): string {
  switch (options.mode) {
    case "existing_session":
      return `已有普通会话 · ${options.sessionId}`;
    case "existing_workflow":
      return `已有工作流会话 · ${options.workflowId}`;
    case "new_session":
      return `新建普通会话 · ${options.agent}@${options.machine} · ${options.workspace}`;
    case "new_workflow":
      return `新建工作流会话 · ${options.plan}`;
  }
}

function runSummary(run: AutomationRun | undefined, includeError = true): string {
  if (run === undefined) return "尚未运行";
  const state = {
    queued: "排队中",
    running: "运行中",
    finished: "已完成",
    failed: "失败",
  }[run.state];
  const parts = [formatTime(run.createdAt), state, run.resultLabel];
  if (includeError && run.error) parts.push(run.error);
  return parts.filter(Boolean).join(" · ");
}

function runTimingSummary(run: AutomationRun): string | null {
  if (run.startedAt !== undefined && run.finishedAt !== undefined) {
    const duration = formatDuration(run.finishedAt - run.startedAt);
    return [
      `${formatTime(run.startedAt)} - ${formatTime(run.finishedAt)}`,
      duration === "" ? "" : `耗时 ${duration}`,
    ]
      .filter(Boolean)
      .join(" ");
  }
  if (run.startedAt !== undefined) return `开始 ${formatTime(run.startedAt)}`;
  if (run.finishedAt !== undefined) return `完成 ${formatTime(run.finishedAt)}`;
  return null;
}

export function AutomationListView() {
  const core = useCore();
  const state = useCoreState();
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [deleting, setDeleting] = useState<Automation | null>(null);

  useEffect(() => {
    void Promise.all([
      refreshAutomations(core),
      refreshNewSession(core),
      resetAutomationTargets(core),
    ]);
  }, [core]);

  const menuItems = (automation: Automation): MenuItem[] => [
    { label: "立即运行", onSelect: () => void triggerAutomation(core, automation) },
    {
      label: automation.enabled ? "暂停" : "恢复",
      onSelect: () => void toggleAutomation(core, automation),
    },
    { label: "删除", danger: true, onSelect: () => setDeleting(automation) },
  ];

  return (
    <div data-slot="automation-list" className="flex min-h-0 flex-1 flex-col">
      <div className="flex shrink-0 items-center justify-between border-b border-border px-4 py-3">
        <h2 className="text-lg font-semibold">自动化列表</h2>
        <Button
          data-slot="new-automation"
          variant="outline"
          size="icon"
          aria-label="新建自动化"
          title="新建自动化"
          onClick={() => showNewAutomation(core)}
        >
          <Plus className="size-4" />
        </Button>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-3 py-3">
        {state.automations.length === 0 ? (
          <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
            暂无自动化
          </div>
        ) : null}
        <div className="flex flex-col gap-1">
          {state.automations.map((automation) => {
            const active =
              state.automationPanel?.kind === "detail" &&
              state.automationPanel.id === automation.id;
            return (
              <button
                key={automation.id}
                type="button"
                data-slot="automation-item"
                data-enabled={automation.enabled ? "true" : "false"}
                onClick={() => showAutomationDetail(core, automation.id)}
                onContextMenu={(event) => {
                  event.preventDefault();
                  setMenu({
                    x: event.clientX,
                    y: event.clientY,
                    items: menuItems(automation),
                  });
                }}
                className={cn(
                  "flex w-full items-center gap-3 rounded-md border border-transparent px-3 py-2 text-left",
                  active ? "border-border bg-accent" : "hover:bg-accent",
                )}
              >
                <Button
                  variant="ghost"
                  size="icon"
                  className="size-8 shrink-0"
                  aria-label={automation.enabled ? "暂停自动化" : "恢复自动化"}
                  title={automation.enabled ? "暂停" : "恢复"}
                  onClick={(event) => {
                    event.stopPropagation();
                    void toggleAutomation(core, automation);
                  }}
                >
                  {automation.enabled ? (
                    <Pause className="size-4" />
                  ) : (
                    <Play className="size-4" />
                  )}
                </Button>
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm font-medium">
                    {automation.title}
                  </span>
                  <span className="mt-0.5 block truncate text-xs text-muted-foreground">
                    {automation.task}
                  </span>
                  <span className="mt-0.5 block truncate text-xs text-muted-foreground">
                    {triggerSummary(automation)}
                  </span>
                  <span className="mt-0.5 block truncate text-xs text-muted-foreground">
                    最近运行：{runSummary(automation.lastRun)}
                  </span>
                </span>
                {automation.trigger.type === "schedule" ? (
                  <Clock3 className="size-4 shrink-0 text-muted-foreground" />
                ) : (
                  <Github className="size-4 shrink-0 text-muted-foreground" />
                )}
              </button>
            );
          })}
        </div>
      </div>

      <ContextMenu state={menu} onClose={() => setMenu(null)} />
      <ConfirmDialog
        open={deleting !== null}
        title={`删除“${deleting?.title ?? ""}”？`}
        description="自动化定义和全部运行记录都会被删除。"
        confirmLabel="删除"
        onCancel={() => setDeleting(null)}
        onConfirm={() => {
          const automation = deleting;
          setDeleting(null);
          if (automation !== null) void deleteAutomation(core, automation);
        }}
      />
    </div>
  );
}

export function AutomationPanelView() {
  const core = useCore();
  const state = useCoreState();
  const panel = state.automationPanel;
  if (panel === null) return null;
  if (panel.kind === "new") {
    return <AutomationEditor core={core} />;
  }
  const automation = state.automations.find((item) => item.id === panel.id);
  if (automation === undefined) {
    return (
      <div className="flex h-full items-center justify-center p-4 text-sm text-muted-foreground">
        自动化不存在
      </div>
    );
  }
  return <AutomationDetail automation={automation} />;
}

function AutomationDetail({ automation }: { automation: Automation }) {
  const core = useCore();
  const state = useCoreState();
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const runsLoadingRef = useRef(false);
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [runs, setRuns] = useState<AutomationRun[]>([]);
  const [runsLoading, setRunsLoading] = useState(false);
  const [runsHasMore, setRunsHasMore] = useState(false);

  const loadRuns = async (offset: number, append: boolean): Promise<void> => {
    if (core.client === null || runsLoadingRef.current) return;
    runsLoadingRef.current = true;
    setRunsLoading(true);
    try {
      const page = await core.client.automationRuns(
        automation.id,
        AUTOMATION_RUNS_PAGE_SIZE,
        offset,
      );
      setRuns((current) => (append ? [...current, ...page.runs] : page.runs));
      setRunsHasMore(page.hasMore);
    } catch (error) {
      core.failure(`读取运行记录失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      runsLoadingRef.current = false;
      setRunsLoading(false);
    }
  };

  useEffect(() => {
    setRuns([]);
    setRunsHasMore(false);
    void loadRuns(0, false);
  }, [automation.id, automation.updatedAt]);

  useEffect(() => {
    const element = scrollRef.current;
    if (element === null || !runsHasMore || runsLoadingRef.current) return;
    if (element.scrollHeight <= element.clientHeight) {
      void loadRuns(runs.length, true);
    }
  }, [runs, runsHasMore]);

  const loadMoreRuns = (element: HTMLDivElement): void => {
    if (
      element.scrollHeight - element.scrollTop - element.clientHeight <=
        RUNS_SCROLL_THRESHOLD &&
      runsHasMore &&
      !runsLoadingRef.current
    ) {
      void loadRuns(runs.length, true);
    }
  };

  if (editing) {
    return (
      <AutomationEditor
        core={core}
        automation={automation}
        onSaved={() => setEditing(false)}
        onCancel={() => setEditing(false)}
      />
    );
  }

  return (
    <div data-slot="automation-detail" className="flex h-full min-h-0 flex-col">
      <PanelHeader title="自动化详情" onClose={() => closeAutomationPanel(core)} />
      <div
        ref={scrollRef}
        className="min-h-0 flex-1 overflow-y-auto p-4"
        onScroll={(event) => loadMoreRuns(event.currentTarget)}
      >
        <div className="min-w-0">
          <h3 className="truncate text-base font-semibold">{automation.title}</h3>
          <p className="mt-1 text-sm text-muted-foreground">
            {automation.enabled ? "已开启" : "已暂停"}
          </p>
        </div>
        <div className="mt-4 flex flex-wrap gap-2">
          <Button
            variant="outline"
            size="sm"
            onClick={() => void triggerAutomation(core, automation)}
          >
            <Play className="size-4" />
            立即运行
          </Button>
          <Button
            variant="outline"
            size="sm"
            onClick={() => void toggleAutomation(core, automation)}
          >
            {automation.enabled ? <Pause className="size-4" /> : <Play className="size-4" />}
            {automation.enabled ? "暂停" : "恢复"}
          </Button>
          <Button variant="outline" size="sm" onClick={() => setEditing(true)}>
            <Pencil className="size-4" />
            编辑
          </Button>
          <Button
            variant="ghost"
            size="sm"
            className="text-destructive hover:text-destructive"
            onClick={() => setDeleting(true)}
          >
            <Trash2 className="size-4" />
            删除
          </Button>
        </div>

        <dl className="mt-5 grid grid-cols-[6rem_minmax(0,1fr)] gap-x-3 gap-y-2 text-sm">
          <Detail label="触发方式" value={triggerSummary(automation)} />
          <Detail label="任务" value={automation.task} />
          <Detail
            label="会话选项"
            value={sessionOptionsSummary(automation.sessionOptions)}
          />
          <Detail
            label="结果分类"
            value={automation.resultLabels
              .map((label) => `${label.name}：${label.description}`)
              .join("；")}
          />
        </dl>

        <h4 className="mt-6 mb-2 text-sm font-semibold">运行记录</h4>
        {runsLoading && runs.length === 0 ? (
          <p className="text-sm text-muted-foreground">加载中…</p>
        ) : null}
        {!runsLoading && runs.length === 0 ? (
          <p className="text-sm text-muted-foreground">暂无运行记录</p>
        ) : null}
        <div className="flex flex-col gap-2">
          {runs.map((run) => (
            <button
              key={run.id}
              type="button"
              className="rounded-md border border-border px-3 py-2 text-left text-sm hover:bg-accent"
              onClick={() => {
                const entry =
                  run.sessionId !== undefined
                    ? state.automationSessions
                        .filter((session) => session.id === run.sessionId)
                        .map((session) => ({ kind: "session" as const, session }))
                        .at(0)
                    : run.workflowId !== undefined
                      ? state.automationWorkflows
                          .filter((workflow) => workflow.id === run.workflowId)
                          .map((workflow) => ({ kind: "workflow" as const, workflow }))
                          .at(0)
                      : undefined;
                if (entry !== undefined) void openEntry(core, entry);
              }}
            >
              <div className="truncate">{runSummary(run, false)}</div>
              {run.error ? (
                <p className="mt-1 truncate text-xs text-destructive" title={run.error}>
                  {run.error}
                </p>
              ) : null}
              {runTimingSummary(run) ? (
                <p className="mt-1 text-xs text-muted-foreground">{runTimingSummary(run)}</p>
              ) : null}
            </button>
          ))}
        </div>
      </div>
      <ConfirmDialog
        open={deleting}
        title={`删除“${automation.title}”？`}
        description="自动化定义和全部运行记录都会被删除。"
        confirmLabel="删除"
        onCancel={() => setDeleting(false)}
        onConfirm={() => {
          setDeleting(false);
          void deleteAutomation(core, automation);
        }}
      />
    </div>
  );
}

function Detail({ label, value }: { label: string; value: string }) {
  return (
    <>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="min-w-0 whitespace-pre-wrap break-words">{value}</dd>
    </>
  );
}

function AutomationEditor({
  core,
  automation,
  onSaved,
  onCancel,
}: {
  core: ReturnType<typeof useCore>;
  automation?: Automation;
  onSaved?: () => void;
  onCancel?: () => void;
}) {
  const state = useCoreState();
  const initial = automation;
  const [title, setTitle] = useState(initial?.title ?? "");
  const [task, setTask] = useState(initial?.task ?? "");
  const [triggerMode, setTriggerMode] = useState<"schedule" | "webhook">(
    initial?.trigger.type === "github_webhook" ? "webhook" : "schedule",
  );
  const initialCron =
    initial?.trigger.type === "schedule" ? initial.trigger.cron : "";
  const [cron, setCron] = useState(initialCron);
  const [cronError, setCronError] = useState<string | null>(
    () => (initialCron.trim() === "" ? null : previewCron(initialCron).error),
  );
  const [cronNextRuns, setCronNextRuns] = useState<Date[]>(
    () => (initialCron.trim() === "" ? [] : previewCron(initialCron).nextRuns),
  );
  const [repository, setRepository] = useState(
    initial?.trigger.type === "github_webhook" ? initial.trigger.repository : "",
  );
  const [events, setEvents] = useState<string[]>(
    initial?.trigger.type === "github_webhook" ? (initial.trigger.events ?? []) : ["push"],
  );
  const [sessionMode, setSessionMode] = useState<EditorSessionMode>(
    initial?.sessionOptions.mode ?? "new_session",
  );
  const [existingSessionId, setExistingSessionId] = useState(
    initial?.sessionOptions.mode === "existing_session"
      ? initial.sessionOptions.sessionId
      : "",
  );
  const [existingWorkflowId, setExistingWorkflowId] = useState(
    initial?.sessionOptions.mode === "existing_workflow"
      ? initial.sessionOptions.workflowId
      : "",
  );
  const [machine, setMachine] = useState(
    initial?.sessionOptions.mode === "new_session"
      ? initial.sessionOptions.machine
      : "",
  );
  const [agent, setAgent] = useState(
    initial?.sessionOptions.mode === "new_session" ? initial.sessionOptions.agent : "",
  );
  const [workspace, setWorkspace] = useState(
    initial?.sessionOptions.mode === "new_session"
      ? initial.sessionOptions.workspace
      : "",
  );
  const [useWorktree, setUseWorktree] = useState(
    initial?.sessionOptions.mode === "new_session"
      ? initial.sessionOptions.useWorktree
      : false,
  );
  const [worktreeBaseBranch, setWorktreeBaseBranch] = useState(
    initial?.sessionOptions.mode === "new_session"
      ? (initial.sessionOptions.worktreeBaseBranch ?? "")
      : "",
  );
  const [sessionProject, setSessionProject] = useState(
    initial?.sessionOptions.mode === "new_session"
      ? (initial.sessionOptions.project ?? "")
      : "",
  );
  const [plan, setPlan] = useState(
    initial?.sessionOptions.mode === "new_workflow"
      ? initial.sessionOptions.plan
      : "",
  );
  const [workflowProject, setWorkflowProject] = useState(
    initial?.sessionOptions.mode === "new_workflow"
      ? (initial.sessionOptions.project ?? "")
      : "",
  );
  const [selectedPlan, setSelectedPlan] = useState<string | null>(null);
  const [resultLabels, setResultLabels] = useState<AutomationResultLabel[]>(
    initial?.resultLabels.map((label) => ({ ...label })) ?? [
      { name: "", description: "" },
      { name: "", description: "" },
    ],
  );
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    void Promise.all([refreshNewSession(core), refreshAutomations(core)]);
  }, [core]);

  const agents =
    state.settings.agents.find((item) => item.machine === machine)?.agents ?? [];
  const sessionValid =
    (sessionMode === "existing_session" && existingSessionId !== "") ||
    (sessionMode === "existing_workflow" && existingWorkflowId !== "") ||
    (sessionMode === "new_session" &&
      machine !== "" &&
      agent !== "" &&
      workspace.trim() !== "") ||
    (sessionMode === "new_workflow" && plan.trim() !== "");
  const labelsValid =
    resultLabels.length >= 2 &&
    resultLabels.every(
      (label) => label.name.trim() !== "" && label.description.trim() !== "",
    );
  const canSave =
    title.trim() !== "" &&
    task.trim() !== "" &&
    sessionValid &&
    labelsValid &&
    (triggerMode === "schedule"
      ? cron.trim() !== "" && cronError === null
      : repository.trim() !== "");

  const save = async (): Promise<void> => {
    if (!canSave || saving) return;
    if (triggerMode === "schedule") {
      const preview = previewCron(cron);
      setCronError(preview.error);
      setCronNextRuns(preview.nextRuns);
      if (preview.error !== null) return;
    }
    const trigger: AutomationTrigger =
      triggerMode === "schedule"
        ? { type: "schedule", cron: cron.trim() }
        : {
            type: "github_webhook",
            repository: repository.trim(),
            events,
          };
    const sessionOptions: AutomationSessionOptions = (() => {
      switch (sessionMode) {
        case "existing_session":
          return { mode: "existing_session", sessionId: existingSessionId };
        case "existing_workflow":
          return { mode: "existing_workflow", workflowId: existingWorkflowId };
        case "new_session":
          return {
            mode: "new_session",
            machine,
            agent,
            workspace: workspace.trim(),
            useWorktree,
            worktreeBaseBranch:
              useWorktree && worktreeBaseBranch.trim() !== ""
                ? worktreeBaseBranch.trim()
                : undefined,
            project: sessionProject === "" ? undefined : sessionProject,
          };
        case "new_workflow":
          return {
            mode: "new_workflow",
            plan: plan.trim(),
            project: workflowProject === "" ? undefined : workflowProject,
          };
      }
    })();
    const input: AutomationInput = {
      title: title.trim(),
      enabled: initial?.enabled ?? true,
      task: task.trim(),
      trigger,
      sessionOptions,
      resultLabels: resultLabels.map((label) => ({
        name: label.name.trim(),
        description: label.description.trim(),
      })),
    };
    setSaving(true);
    const saved = await saveAutomation(core, input, initial?.id);
    setSaving(false);
    if (saved) onSaved?.();
  };

  return (
    <div data-slot="automation-editor" className="flex h-full min-h-0 flex-col">
      <PanelHeader
        title={initial === undefined ? "新建自动化" : "编辑自动化"}
        onClose={() => {
          if (onCancel !== undefined) onCancel();
          else closeAutomationPanel(core);
        }}
      />
      <div className="min-h-0 flex-1 space-y-4 overflow-y-auto p-4">
        <Field label="标题">
          <Input value={title} onChange={(event) => setTitle(event.target.value)} />
        </Field>
        <Field label="任务">
          <Textarea
            value={task}
            onChange={(event) => setTask(event.target.value)}
            className="min-h-24"
          />
        </Field>
        <div className="space-y-4 border-t border-border pt-4">
          <Field label="触发方式">
            <Tabs
              value={triggerMode}
              onValueChange={(value) => setTriggerMode(value as "schedule" | "webhook")}
            >
              <TabsList className="w-full">
                <TabsTrigger className="flex-1" value="schedule">
                  定时
                </TabsTrigger>
                <TabsTrigger className="flex-1" value="webhook">
                  Github Webhook
                </TabsTrigger>
              </TabsList>
            </Tabs>
          </Field>
          {triggerMode === "schedule" ? (
            <Field label="cron 表达式">
              <div className="space-y-2">
                <Input
                  value={cron}
                  onChange={(event) => {
                    setCron(event.target.value);
                    setCronError(null);
                    setCronNextRuns([]);
                  }}
                  onBlur={() => {
                    const preview = previewCron(cron);
                    setCronError(preview.error);
                    setCronNextRuns(preview.nextRuns);
                  }}
                  placeholder="0 9 * * *"
                />
                {cronError !== null ? (
                  <p className="text-xs text-destructive">{cronError}</p>
                ) : cronNextRuns.length > 0 ? (
                  <div className="space-y-1">
                    <p className="text-xs text-muted-foreground">最近 5 次触发时间</p>
                    {cronNextRuns.map((time) => (
                      <p key={time.getTime()} className="text-xs text-muted-foreground">
                        {formatTime(time.getTime())}
                      </p>
                    ))}
                  </div>
                ) : null}
              </div>
            </Field>
          ) : (
            <div className="grid gap-4">
              <Field label="仓库">
                <Input
                  value={repository}
                  onChange={(event) => setRepository(event.target.value)}
                  placeholder="owner/repository"
                />
              </Field>
              <Field label="推送事件">
                <MultiSelectCombobox
                  options={GITHUB_WEBHOOK_EVENTS}
                  value={events}
                  onChange={setEvents}
                  placeholder="全部事件"
                  ariaLabel="选择推送事件"
                />
              </Field>
            </div>
          )}
        </div>

        <div className="space-y-4 border-t border-border pt-4">
          <Field label="会话选项">
          <Tabs
            value={sessionMode}
            onValueChange={(value) => setSessionMode(value as EditorSessionMode)}
          >
            <TabsList className="w-full flex-wrap">
              <TabsTrigger className="flex-1" value="existing_session">
                已有普通会话
              </TabsTrigger>
              <TabsTrigger className="flex-1" value="existing_workflow">
                已有工作流会话
              </TabsTrigger>
              <TabsTrigger className="flex-1" value="new_session">
                新建普通会话
              </TabsTrigger>
              <TabsTrigger className="flex-1" value="new_workflow">
                新建工作流会话
              </TabsTrigger>
            </TabsList>
          </Tabs>
        </Field>
        {sessionMode === "existing_session" ? (
          <Field label="普通会话">
            <Select
              value={existingSessionId || "none"}
              onValueChange={(value) => setExistingSessionId(value === "none" ? "" : value)}
            >
              <SelectTrigger className="w-full">
                <SelectValue placeholder="选择普通会话" />
              </SelectTrigger>
              <SelectContent
                className="w-[var(--radix-select-trigger-width)]"
                onViewportScroll={(event) => {
                  const node = event.currentTarget;
                  if (node.scrollTop + node.clientHeight >= node.scrollHeight - 24) {
                    void loadMoreAutomationSessions(core);
                  }
                }}
              >
                <SelectItem value="none">未选择</SelectItem>
                {state.automationSessions.map((session) => (
                  <SelectItem key={session.id} value={session.id}>
                    {(session.pinned ? "置顶 · " : "") + (session.title || session.id)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>
        ) : null}
        {sessionMode === "existing_workflow" ? (
          <Field label="工作流会话">
            <Select
              value={existingWorkflowId || "none"}
              onValueChange={(value) => setExistingWorkflowId(value === "none" ? "" : value)}
            >
              <SelectTrigger className="w-full">
                <SelectValue placeholder="选择工作流会话" />
              </SelectTrigger>
              <SelectContent
                className="w-[var(--radix-select-trigger-width)]"
                onViewportScroll={(event) => {
                  const node = event.currentTarget;
                  if (node.scrollTop + node.clientHeight >= node.scrollHeight - 24) {
                    void loadMoreAutomationWorkflows(core);
                  }
                }}
              >
                <SelectItem value="none">未选择</SelectItem>
                {state.automationWorkflows.map((workflow) => (
                  <SelectItem key={workflow.id} value={workflow.id}>
                    {(workflow.pinned ? "置顶 · " : "") + (workflow.title || workflow.id)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>
        ) : null}
        {sessionMode === "new_session" ? (
          <div className="space-y-4">
            <Field label="机器">
              <OptionGroup>
                {state.settings.machines.map((item) => (
                  <OptionButton
                    key={item.name}
                    selected={machine === item.name}
                    onClick={() => {
                      setMachine(item.name);
                      setAgent("");
                    }}
                  >
                    {item.name}
                  </OptionButton>
                ))}
              </OptionGroup>
            </Field>
            <Field label="智能体">
              <OptionGroup>
                {agents.map((item) => (
                  <OptionButton
                    key={item.name}
                    selected={agent === item.name}
                    disabled={!item.available}
                    onClick={() => setAgent(item.name)}
                  >
                    {item.name}
                  </OptionButton>
                ))}
              </OptionGroup>
            </Field>
            <Field label="工作目录">
              <WorkspaceField
                core={core}
                machine={machine}
                value={workspace}
                onChange={setWorkspace}
                onPickRecent={(item) => {
                  if (
                    item.lastUsedProject !== undefined &&
                    state.settings.projects.some(
                      (project) => project.name === item.lastUsedProject,
                    )
                  ) {
                    setSessionProject(item.lastUsedProject);
                  }
                }}
                slotPrefix="automation-workspace"
              />
            </Field>
            <Field label="项目">
              <OptionGroup>
                {state.settings.projects.map((item) => (
                  <OptionButton
                    key={item.name}
                    selected={sessionProject === item.name}
                    onClick={() =>
                      setSessionProject(sessionProject === item.name ? "" : item.name)
                    }
                  >
                    {item.name}
                  </OptionButton>
                ))}
              </OptionGroup>
            </Field>
            <div className="flex items-center justify-between gap-3">
              <Label>使用 worktree</Label>
              <Switch checked={useWorktree} onCheckedChange={setUseWorktree} />
            </div>
            {useWorktree ? (
              <WorktreeBranchField
                core={core}
                machine={machine}
                workspace={workspace}
                value={worktreeBaseBranch}
                onChange={setWorktreeBaseBranch}
              />
            ) : null}
          </div>
        ) : null}
        {sessionMode === "new_workflow" ? (
          <div className="space-y-4">
            <Field label="工作流计划">
              <WorkflowPlanField
                plans={state.settings.plans}
                value={plan}
                selectedPlan={selectedPlan}
                onChange={(value) => {
                  setPlan(value);
                  setSelectedPlan(null);
                }}
                onSelectPlan={(item) => {
                  setPlan(item.plan);
                  setSelectedPlan(item.name);
                  if (
                    item.lastUsedProject !== undefined &&
                    state.settings.projects.some(
                      (project) => project.name === item.lastUsedProject,
                    )
                  ) {
                    setWorkflowProject(item.lastUsedProject);
                  }
                }}
                slotPrefix="automation-workflow-plan"
              />
            </Field>
            <Field label="项目">
              <OptionGroup>
                {state.settings.projects.map((item) => (
                  <OptionButton
                    key={item.name}
                    selected={workflowProject === item.name}
                    onClick={() =>
                      setWorkflowProject(workflowProject === item.name ? "" : item.name)
                    }
                  >
                    {item.name}
                  </OptionButton>
                ))}
              </OptionGroup>
            </Field>
          </div>
        ) : null}
        </div>

        <div className="space-y-4 border-t border-border pt-4">
          <Field label="运行结果分类">
            <div className="space-y-2">
              {resultLabels.map((label, index) => (
                <div key={index} className="grid gap-2 rounded-md border border-border p-2">
                  <Input
                    value={label.name}
                    placeholder="标签名"
                    onChange={(event) =>
                      setResultLabels((current) =>
                        current.map((item, itemIndex) =>
                          itemIndex === index ? { ...item, name: event.target.value } : item,
                        ),
                      )
                    }
                  />
                  <Input
                    value={label.description}
                    placeholder="标签描述"
                    onChange={(event) =>
                      setResultLabels((current) =>
                        current.map((item, itemIndex) =>
                          itemIndex === index
                            ? { ...item, description: event.target.value }
                            : item,
                        ),
                      )
                    }
                  />
                  {resultLabels.length > 2 ? (
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() =>
                        setResultLabels((current) =>
                          current.filter((_, itemIndex) => itemIndex !== index),
                        )
                      }
                    >
                      删除分类
                    </Button>
                  ) : null}
                </div>
              ))}
              <Button
                variant="outline"
                size="sm"
                onClick={() =>
                  setResultLabels((current) => [
                    ...current,
                    { name: "", description: "" },
                  ])
                }
              >
                添加分类
              </Button>
            </div>
          </Field>

          <Button className="w-full" disabled={!canSave || saving} onClick={() => void save()}>
            {saving ? "保存中…" : "保存"}
          </Button>
        </div>
      </div>
    </div>
  );
}

function PanelHeader({ title, onClose }: { title: string; onClose: () => void }) {
  return (
    <div className="flex shrink-0 items-center justify-between border-b border-border px-4 py-2">
      <h2 className="text-sm font-semibold">{title}</h2>
      <Button variant="ghost" size="icon" aria-label="关闭" title="关闭" onClick={onClose}>
        <X />
      </Button>
    </div>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="space-y-1.5">
      <Label className="text-muted-foreground">{label}</Label>
      {children}
    </div>
  );
}

function OptionGroup({ children }: { children: ReactNode }) {
  return <div className="flex flex-wrap gap-1.5">{children}</div>;
}

function OptionButton({
  selected,
  disabled = false,
  onClick,
  children,
}: {
  selected: boolean;
  disabled?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      aria-pressed={selected}
      disabled={disabled}
      onClick={onClick}
      className={cn(
        "min-h-9 rounded-sm border border-border bg-card px-2.5 py-1 text-xs",
        selected && "border-primary bg-accent",
        disabled ? "cursor-not-allowed opacity-50" : "cursor-pointer hover:bg-accent",
      )}
    >
      {children}
    </button>
  );
}
