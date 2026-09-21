// 新建会话视图（docs/PRD.md「新建会话视图」、docs/DESIGN.md「新建会话视图」）。
//
// 顶部为模式切换（普通/工作流）：普通模式选择机器与可用 agent、输入工作目录（前缀匹配联想与最近目录）、
// worktree 开关；工作流模式选择或输入工作计划（内置智能体未配置时引导去设置）。

import { useEffect, useRef, useState, type ReactNode } from "react";
import { Folder } from "lucide-react";

import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { Label } from "../components/ui/label";
import { Switch } from "../components/ui/switch";
import { Tabs, TabsList, TabsTrigger } from "../components/ui/tabs";
import { Textarea } from "../components/ui/textarea";
import { createSession, createWorkflow, openSettings, updateWorkspaceInput } from "../core/actions";
import { refreshWorkflowSetup } from "../core/poll";
import { useCore, useCoreState } from "../core/store";
import { truncate } from "../lib/format";
import { cn } from "../lib/utils";

/** 已保存计划上拉框：向上弹出，上方空间不足时向下弹，高度按剩余空间收敛。 */
type PlanPopupPlacement = { below: boolean; maxHeight: number };

/** 工作目录字段的浮层：点击输入框展示最近目录（上拉），手动输入改为前缀匹配（下拉）。 */
type WorkspacePopup = { kind: "recent" | "suggest"; maxHeight: number };

/** 浮层最大高度（对应 max-h-56）。 */
const PLAN_POPUP_HEIGHT = 224;
/** 浮层与锚点的间距。 */
const POPUP_GAP = 8;
/** 浮层最小高度：空间实在不够时允许略超出视口，也不至于只露一条缝。 */
const POPUP_MIN_HEIGHT = 80;

/** 锚点某一侧可用于浮层的高度：收敛到该方向剩余空间。 */
function popupMaxHeight(anchor: DOMRect, below: boolean): number {
  const available = below ? window.innerHeight - anchor.bottom - POPUP_GAP : anchor.top - POPUP_GAP;
  return Math.max(Math.min(PLAN_POPUP_HEIGHT, available), POPUP_MIN_HEIGHT);
}

/**
 * 选项按钮组中的一个按钮（docs/PRD.md「新建会话视图」：机器与 agent 均采用选项按钮组，
 * 不可用的 agent 置灰不可点击）。与桌面应用 `Button::small().selected()` 的外观一致。
 */
function OptionButton({
  slot,
  selected,
  disabled = false,
  available = true,
  onClick,
  children,
}: {
  slot: "machine-option" | "agent-option";
  selected: boolean;
  disabled?: boolean;
  available?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      data-slot={slot}
      data-selected={selected ? "true" : "false"}
      data-available={available ? "true" : "false"}
      aria-pressed={selected}
      disabled={disabled}
      onClick={onClick}
      className={cn(
        "min-h-10 rounded-sm border border-border bg-card px-2.5 py-2 text-xs lg:min-h-0 lg:px-2 lg:py-1",
        selected && "border-primary bg-accent",
        disabled ? "cursor-not-allowed opacity-50" : "cursor-pointer hover:bg-accent",
      )}
    >
      {children}
    </button>
  );
}

export function NewSessionView() {
  const core = useCore();
  const state = useCoreState();
  const planField = useRef<HTMLDivElement | null>(null);
  const [planPopup, setPlanPopup] = useState<PlanPopupPlacement | null>(null);
  const [workspacePopup, setWorkspacePopup] = useState<WorkspacePopup | null>(null);
  const { mode, machine, agent, workspace, useWorktree, plan, selectedPlan, suggestions } =
    state.newSession;
  const agents = state.settings.agents.find((item) => item.machine === machine)?.agents ?? [];
  const selectedAgent = agents.find((item) => item.name === agent);
  // PRD「新建会话视图」：已选择机器、已选择可用 agent、工作目录非空才可创建
  const canCreate =
    machine !== "" &&
    selectedAgent !== undefined &&
    selectedAgent.available &&
    workspace.trim() !== "";
  const recent = state.recentWorkspaces
    .filter((item) => item.machine === machine)
    .sort((a, b) => b.lastUsed - a.lastUsed)
    .slice(0, 8);

  // 切换模式后不保留浮层：回到工作流/普通模式时由聚焦输入框重新触发
  useEffect(() => {
    setPlanPopup(null);
    setWorkspacePopup(null);
  }, [mode]);

  // 已保存计划的上拉框按点击外部收起（工作目录的两个浮层由输入框失焦收起）
  useEffect(() => {
    const onMouseDown = (event: MouseEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      const plans = planField.current;
      if (plans !== null && !plans.contains(target)) setPlanPopup(null);
    };
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
  }, []);

  /** 点击/聚焦工作目录输入框：上拉展示最近使用的工作目录。 */
  const showRecentWorkspaces = (input: HTMLInputElement): void => {
    setWorkspacePopup(
      recent.length === 0
        ? null
        : { kind: "recent", maxHeight: popupMaxHeight(input.getBoundingClientRect(), false) },
    );
  };

  /** 展开已保存计划上拉框：按输入框上下的可用空间决定方向与高度。 */
  const openPlanPopup = (input: HTMLElement): void => {
    const rect = input.getBoundingClientRect();
    const above = rect.top - POPUP_GAP;
    const below = window.innerHeight - rect.bottom - POPUP_GAP;
    const useBelow = above < PLAN_POPUP_HEIGHT && below > above;
    setPlanPopup({ below: useBelow, maxHeight: popupMaxHeight(rect, useBelow) });
  };

  /** 选中已保存计划：填入输入框并收起上拉框。 */
  const pickPlan = (name: string, text: string): void => {
    core.update((draft) => {
      draft.newSession.plan = text;
      draft.newSession.selectedPlan = name;
    });
    setPlanPopup(null);
  };

  /** 选中最近使用的工作目录：填入并收起浮层（选定项，不再联想）。 */
  const pickRecentWorkspace = (value: string): void => {
    core.update((draft) => {
      draft.newSession.workspace = value;
      draft.newSession.suggestions = [];
    });
    setWorkspacePopup(null);
  };

  /**
   * 选中前缀匹配的目录项：填入该目录并保持下拉框，由联想结果继续展示这一级目录项，
   * 用户可一路点选下钻（docs/DESIGN.md「新建会话视图」：目录边界处拉取该目录的全部目录项）。
   */
  const pickSuggestion = (value: string): void => {
    const path = value.endsWith("/") ? value : `${value}/`;
    core.update((draft) => {
      draft.newSession.workspace = path;
      draft.newSession.suggestions = [];
    });
    void updateWorkspaceInput(core, path);
  };

  const noMachines = state.settings.machines.length === 0;
  const normalForm = (
    <div className="flex flex-col gap-4">
      <div className="flex flex-col gap-2">
        <Label>机器</Label>
        <div data-slot="machine-group" className="flex flex-wrap gap-1">
          {state.settings.machines.map((item) => (
            <OptionButton
              key={item.name}
              slot="machine-option"
              selected={machine === item.name}
              onClick={() => {
                core.update((draft) => {
                  draft.newSession.machine = item.name;
                  draft.newSession.agent = "";
                });
                void updateWorkspaceInput(core, workspace);
              }}
            >
              {item.name}
            </OptionButton>
          ))}
        </div>
      </div>

      <div className="flex flex-col gap-2">
        <Label>智能体</Label>
        {agents.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            {machine === "" ? "请选择机器" : "该机器未发现智能体"}
          </p>
        ) : (
          <div data-slot="agent-group" className="flex flex-wrap gap-1">
            {agents.map((item) => (
              <OptionButton
                key={item.name}
                slot="agent-option"
                selected={agent === item.name}
                available={item.available}
                disabled={!item.available}
                onClick={() =>
                  core.update((draft) => {
                    draft.newSession.agent = item.name;
                  })
                }
              >
                {item.name}
              </OptionButton>
            ))}
          </div>
        )}
      </div>

      <div className="flex flex-col gap-2">
        <Label>工作目录</Label>
        <div className="relative">
          <Input
            data-slot="workspace-input"
            aria-label="工作目录"
            value={workspace}
            onFocus={(event) => showRecentWorkspaces(event.currentTarget)}
            onClick={(event) => showRecentWorkspaces(event.currentTarget)}
            onBlur={() => setWorkspacePopup(null)}
            onChange={(event) => {
              const value = event.target.value;
              core.update((draft) => {
                draft.newSession.workspace = value;
                // 输入变化先清掉旧候选：新前缀的应答到达前不展示与当前输入不符的目录项
                draft.newSession.suggestions = [];
              });
              void updateWorkspaceInput(core, value);
              // 手动输入：上拉框（最近目录）换成下拉框（前缀匹配）
              setWorkspacePopup({
                kind: "suggest",
                maxHeight: popupMaxHeight(event.currentTarget.getBoundingClientRect(), true),
              });
            }}
          />
          {workspacePopup?.kind === "recent" && recent.length > 0 ? (
            <div
              data-slot="recent-workspaces"
              style={{ maxHeight: workspacePopup.maxHeight }}
              className="absolute bottom-full left-0 z-10 mb-1 flex w-full flex-col overflow-y-auto rounded-md border border-border bg-popover py-1 shadow-lg"
            >
              {recent.map((item) => (
                <button
                  key={item.workspace}
                  type="button"
                  data-slot="recent-workspace"
                  // 阻止默认行为以免输入框失焦（失焦会收起浮层，点击就落不到这一项上）
                  onMouseDown={(event) => event.preventDefault()}
                  onClick={() => pickRecentWorkspace(item.workspace)}
                  className="cursor-pointer truncate px-2 py-2 text-left text-sm hover:bg-accent lg:py-1"
                >
                  {item.workspace}
                </button>
              ))}
            </div>
          ) : null}
          {workspacePopup?.kind === "suggest" && suggestions.length > 0 ? (
            <div
              data-slot="workspace-suggestions"
              style={{ maxHeight: workspacePopup.maxHeight }}
              className="absolute top-full left-0 z-10 mt-1 flex w-full flex-col overflow-y-auto rounded-md border border-border bg-popover p-1 shadow-lg"
            >
              {suggestions.map((item) => (
                <button
                  key={item.path}
                  type="button"
                  data-slot="workspace-suggestion"
                  data-path={item.path}
                  onMouseDown={(event) => event.preventDefault()}
                  onClick={() => pickSuggestion(item.path)}
                  className="flex cursor-pointer items-center gap-1.5 rounded-sm px-2 py-2 text-left text-sm hover:bg-accent lg:py-1"
                >
                  <Folder className="size-4 shrink-0 text-muted-foreground" />
                  <span className="truncate">{item.name}</span>
                </button>
              ))}
            </div>
          ) : null}
        </div>
      </div>

      <div className="flex items-center justify-between">
        <Label>使用 worktree</Label>
        <Switch
          data-slot="worktree-switch"
          aria-label="使用 worktree"
          checked={useWorktree}
          onCheckedChange={(checked) =>
            core.update((draft) => {
              draft.newSession.useWorktree = checked;
            })
          }
        />
      </div>

      <Button data-slot="create-session" disabled={!canCreate} onClick={() => void createSession(core)}>
        创建会话
      </Button>
    </div>
  );

  // PRD「新建会话视图」：确认内置智能体已配置后才展示创建表单
  const orchestrator = state.settings.orchestrator;
  const workflowForm =
    orchestrator.status === "ready" && orchestrator.config !== null ? (
      <div className="flex flex-col gap-3">
        <div className="flex flex-col gap-2">
          <Label>工作计划</Label>
          <div ref={planField} className="relative">
            {/* 已保存计划的上拉框：聚焦输入框时弹出，覆盖在上方不挤占表单版面 */}
            {planPopup !== null && state.settings.plans.length > 0 ? (
              <div
                data-slot="plan-popup"
                data-direction={planPopup.below ? "below" : "above"}
                style={{ maxHeight: planPopup.maxHeight }}
                className={cn(
                  "absolute left-0 z-10 flex w-full flex-col overflow-y-auto rounded-md border border-border bg-popover py-1 shadow-lg",
                  planPopup.below ? "top-full mt-1" : "bottom-full mb-1",
                )}
              >
                {state.settings.plans.map((item) => (
                  <button
                    key={item.name}
                    type="button"
                    data-slot="plan-choice"
                    data-selected={selectedPlan === item.name ? "true" : "false"}
                    onClick={() => pickPlan(item.name, item.plan)}
                    className={cn(
                      "flex cursor-pointer flex-col items-start gap-1 px-2 py-2 text-left hover:bg-accent lg:py-1",
                      selectedPlan === item.name && "bg-accent",
                    )}
                  >
                    <span className="text-sm">{item.name}</span>
                    <span className="text-xs text-muted-foreground">{truncate(item.plan, 80)}</span>
                  </button>
                ))}
              </div>
            ) : null}
            <Textarea
              data-slot="workflow-plan-input"
              aria-label="工作计划"
              className="min-h-32"
              placeholder="输入工作计划"
              value={plan}
              onFocus={(event) => openPlanPopup(event.currentTarget)}
              onKeyDown={(event) => {
                if (event.key === "Escape") setPlanPopup(null);
              }}
              onChange={(event) => {
                const value = event.target.value;
                core.update((draft) => {
                  draft.newSession.plan = value;
                  draft.newSession.selectedPlan = null;
                });
              }}
            />
          </div>
        </div>
        <Button
          data-slot="create-workflow"
          disabled={plan.trim() === ""}
          onClick={() => void createWorkflow(core)}
        >
          创建会话
        </Button>
      </div>
    ) : (
      <div
        data-slot="orchestrator-unavailable"
        className="flex flex-col gap-3 rounded-md border border-destructive/50 bg-card p-3"
      >
        {orchestrator.status === "loading" ? (
          <p data-slot="orchestrator-loading" className="text-muted-foreground">
            正在确认内置智能体配置…
          </p>
        ) : orchestrator.status === "failed" ? (
          <>
            <p data-slot="orchestrator-error" className="text-destructive">
              读取内置智能体配置失败：{orchestrator.error}
            </p>
            <div className="flex gap-2">
              <Button
                data-slot="retry-orchestrator"
                variant="outline"
                onClick={() => void refreshWorkflowSetup(core)}
              >
                重试
              </Button>
              <Button variant="outline" onClick={() => openSettings(core, "orchestrator")}>
                前往设置
              </Button>
            </div>
          </>
        ) : (
          <>
            <p className="text-destructive">内置智能体未配置，无法创建工作流会话。</p>
            <Button
              data-slot="open-orchestrator-settings"
              variant="outline"
              onClick={() => openSettings(core, "orchestrator")}
            >
              前往设置
            </Button>
          </>
        )}
      </div>
    );

  const noNormalMachines = mode === "normal" && noMachines;

  return (
    <div data-slot="new-session-view" className="flex h-full overflow-auto p-4 lg:p-6">
      {/* PRD「新建会话视图」：居中展示。用 m-auto 而非 justify-center，
          表单比面板高时不会被裁掉顶部、仍可从上往下滚动 */}
      <div className={cn("flex w-full max-w-lg flex-col gap-4", noNormalMachines ? "h-full" : "m-auto")}>
        <Tabs
          data-slot="new-session-mode"
          value={mode}
          onValueChange={(value) => {
            const next = value === "workflow" ? "workflow" : "normal";
            core.update((draft) => {
              draft.newSession.mode = next;
            });
            if (next === "workflow") void refreshWorkflowSetup(core);
          }}
        >
          <TabsList className="w-48 self-center">
            <TabsTrigger data-slot="mode-normal" value="normal">
              普通
            </TabsTrigger>
            <TabsTrigger data-slot="mode-workflow" value="workflow">
              工作流
            </TabsTrigger>
          </TabsList>
        </Tabs>
        {noNormalMachines ? (
          <div className="flex min-h-0 flex-1 flex-col items-center justify-center">
            <p data-slot="no-machines" className="text-sm text-muted-foreground">
              请运行 amux-daemon 程序将机器连接至服务器
            </p>
          </div>
        ) : mode === "normal" ? (
          normalForm
        ) : (
          workflowForm
        )}
      </div>
    </div>
  );
}
