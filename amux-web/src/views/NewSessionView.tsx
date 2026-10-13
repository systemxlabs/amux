// 新建会话视图（docs/PRD.md「新建会话视图」、docs/DESIGN.md「新建会话视图」）。
//
// 顶部为模式切换（普通/工作流）：普通模式选择机器与可用 agent、输入工作目录（前缀匹配联想与最近目录）、
// worktree 开关；工作流模式选择或输入工作计划（内置智能体未配置时引导去设置）。

import type { ReactNode } from "react";

import {
  WorkflowPlanField,
  WorkspaceField,
  WorktreeBranchField,
} from "../components/SessionCreationFields";
import { Button } from "../components/ui/button";
import { Label } from "../components/ui/label";
import { Switch } from "../components/ui/switch";
import { Tabs, TabsList, TabsTrigger } from "../components/ui/tabs";
import { createSession, createWorkflow, openSettings } from "../core/actions";
import { refreshWorkflowSetup } from "../core/poll";
import { useCore, useCoreState } from "../core/store";
import { cn } from "../lib/utils";

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
  slot: "machine-option" | "agent-option" | "project-option";
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
  const {
    mode,
    machine,
    agent,
    workspace,
    useWorktree,
    worktreeBaseBranch,
    plan,
    selectedPlan,
  } = state.newSession;
  const agents = state.settings.agents.find((item) => item.machine === machine)?.agents ?? [];
  const selectedAgent = agents.find((item) => item.name === agent);
  // PRD「新建会话视图」：已选择机器、已选择可用 agent、工作目录非空才可创建
  const canCreate =
    machine !== "" &&
    selectedAgent !== undefined &&
    selectedAgent.available &&
    workspace.trim() !== "";
  /** 选中已保存计划：沿用新建会话行为，同时恢复该计划最近使用的项目。 */
  const pickPlan = (item: (typeof state.settings.plans)[number]): void => {
    core.update((draft) => {
      draft.newSession.plan = item.plan;
      draft.newSession.selectedPlan = item.name;
      draft.newSession.project =
        item.lastUsedProject !== undefined &&
        state.settings.projects.some((project) => project.name === item.lastUsedProject)
          ? item.lastUsedProject
          : undefined;
    });
  };

  /** 选中最近使用的工作目录：沿用新建会话行为，同时恢复该项目。 */
  const pickRecentWorkspace = (item: (typeof state.recentWorkspaces)[number]): void => {
    core.update((draft) => {
      draft.newSession.workspace = item.workspace;
      draft.newSession.project =
        item.lastUsedProject !== undefined &&
        state.settings.projects.some((project) => project.name === item.lastUsedProject)
          ? item.lastUsedProject
          : undefined;
    });
  };

  // PRD「新建会话视图」：无连接机器时整个视图展示提示，与普通/工作流模式无关
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
        <WorkspaceField
          core={core}
          machine={machine}
          value={workspace}
          onPickRecent={pickRecentWorkspace}
          onChange={(value) => {
            core.update((draft) => {
              draft.newSession.workspace = value;
            });
          }}
        />
      </div>

      <div className="flex flex-col gap-2">
        <Label>项目</Label>
        <div data-slot="project-group" className="flex flex-wrap gap-1">
          {state.settings.projects.map((project) => (
            <OptionButton
              key={project.name}
              slot="project-option"
              selected={state.newSession.project === project.name}
              onClick={() =>
                core.update((draft) => {
                  draft.newSession.project =
                    draft.newSession.project === project.name ? undefined : project.name;
                })
              }
            >
              {project.name}
            </OptionButton>
          ))}
        </div>
      </div>

      <div className="flex flex-col gap-2">
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
        {useWorktree ? (
          <WorktreeBranchField
            core={core}
            machine={machine}
            workspace={workspace}
            value={worktreeBaseBranch}
            onChange={(value) => {
              core.update((draft) => {
                draft.newSession.worktreeBaseBranch = value;
              });
            }}
          />
        ) : null}
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
          <WorkflowPlanField
            plans={state.settings.plans}
            value={plan}
            selectedPlan={selectedPlan}
            onChange={(value) => {
              core.update((draft) => {
                draft.newSession.plan = value;
                draft.newSession.selectedPlan = null;
              });
            }}
            onSelectPlan={pickPlan}
          />
        </div>
        <div className="flex flex-col gap-2">
          <Label>项目</Label>
          <div data-slot="workflow-project-group" className="flex flex-wrap gap-1">
            {state.settings.projects.map((project) => (
              <OptionButton
                key={project.name}
                slot="project-option"
                selected={state.newSession.project === project.name}
                onClick={() =>
                  core.update((draft) => {
                    draft.newSession.project =
                      draft.newSession.project === project.name ? undefined : project.name;
                  })
                }
              >
                {project.name}
              </OptionButton>
            ))}
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
            正在确认工作流智能体配置…
          </p>
        ) : orchestrator.status === "failed" ? (
          <>
            <p data-slot="orchestrator-error" className="text-destructive">
              读取工作流智能体配置失败：{orchestrator.error}
            </p>
            <div className="flex gap-2">
              <Button
                data-slot="retry-orchestrator"
                variant="outline"
                onClick={() => void refreshWorkflowSetup(core)}
              >
                重试
              </Button>
              <Button variant="outline" onClick={() => openSettings(core, "workflow")}>
                前往设置
              </Button>
            </div>
          </>
        ) : (
          <>
            <p className="text-destructive">工作流智能体未配置，无法创建工作流会话。</p>
            <Button
              data-slot="open-orchestrator-settings"
              variant="outline"
              onClick={() => openSettings(core, "workflow")}
            >
              前往设置
            </Button>
          </>
        )}
      </div>
    );

  return (
    <div data-slot="new-session-view" className="flex h-full overflow-auto p-4 lg:p-6">
      {/* PRD「新建会话视图」：无连接机器时整个视图仅展示居中提示 */}
      {noMachines ? (
        <div className="m-auto">
          <p data-slot="no-machines" className="text-center text-sm text-muted-foreground">
            请运行 amux-daemon 程序将机器连接至服务器
          </p>
        </div>
      ) : (
        <div className="m-auto flex w-full max-w-lg flex-col gap-4">
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
          {mode === "normal" ? (
            normalForm
          ) : (
            workflowForm
          )}
        </div>
      )}
    </div>
  );
}
