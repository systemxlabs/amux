// 新建会话视图（docs/PRD.md「新建会话视图」、docs/DESIGN.md「新建会话视图」）。
//
// 顶部为模式切换（普通/工作流）：普通模式选择机器与可用 agent、输入工作目录（前缀匹配联想与最近目录）、
// worktree 开关；工作流模式选择或输入工作计划（编排智能体未配置时引导去设置）。

import { useEffect } from "react";
import { Folder } from "lucide-react";

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
import { createSession, createWorkflow, openSettings, updateWorkspaceInput } from "../core/actions";
import { refreshNewSession, refreshWorkflowSetup } from "../core/poll";
import { useCore, useCoreState } from "../core/store";
import { truncate } from "../lib/format";
import { cn } from "../lib/utils";

export function NewSessionView() {
  const core = useCore();
  const state = useCoreState();
  const { mode, machine, agent, workspace, useWorktree, plan, selectedPlan, suggestions, recentOpen } =
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
  const showRecent = machine !== "" && recent.length > 0 && (workspace.trim() === "" || recentOpen);

  useEffect(() => {
    if (state.settings.machines.length === 0) void refreshNewSession(core);
  }, [core, state.settings.machines.length]);

  /** 填入工作目录并按其内容刷新前缀匹配候选。 */
  const pickWorkspace = (value: string): void => {
    core.update((draft) => {
      draft.newSession.workspace = value;
      draft.newSession.recentOpen = false;
    });
    void updateWorkspaceInput(core, value);
  };

  const normalForm = (
    <div className="flex flex-col gap-4">
      <div className="flex flex-col gap-2">
        <Label>机器</Label>
        {/* 空串即未选择，Radix 会以此展示 placeholder */}
        <Select
          value={machine}
          onValueChange={(value) => {
            core.update((draft) => {
              draft.newSession.machine = value;
              draft.newSession.agent = "";
            });
            void updateWorkspaceInput(core, workspace);
          }}
        >
          <SelectTrigger data-slot="machine-select" aria-label="机器" className="w-full">
            <SelectValue placeholder="选择机器" />
          </SelectTrigger>
          <SelectContent>
            {state.settings.machines.map((item) => (
              <SelectItem key={item.name} value={item.name}>
                {item.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      <div className="flex flex-col gap-2">
        <Label>agent</Label>
        {agents.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            {machine === "" ? "请先选择机器" : "该机器未发现 agent"}
          </p>
        ) : (
          <div className="flex flex-col gap-1">
            {agents.map((item) => (
              <button
                key={item.name}
                type="button"
                data-slot="agent-option"
                data-available={item.available ? "true" : "false"}
                data-selected={agent === item.name ? "true" : "false"}
                disabled={!item.available}
                onClick={() =>
                  core.update((draft) => {
                    draft.newSession.agent = item.name;
                  })
                }
                className={cn(
                  "rounded-md border border-border bg-card px-2.5 py-1.5 text-left text-sm",
                  item.available ? "cursor-pointer hover:bg-accent" : "cursor-not-allowed opacity-50",
                  agent === item.name && "border-primary",
                )}
              >
                {item.name}
              </button>
            ))}
          </div>
        )}
      </div>

      <div className="flex flex-col gap-2">
        <div className="flex items-center justify-between">
          <Label>工作目录</Label>
          {machine !== "" && recent.length > 0 ? (
            <Button
              data-slot="recent-toggle"
              variant="ghost"
              size="sm"
              onClick={() =>
                core.update((draft) => {
                  draft.newSession.recentOpen = !draft.newSession.recentOpen;
                })
              }
            >
              最近使用的工作目录
            </Button>
          ) : null}
        </div>
        <Input
          data-slot="workspace-input"
          aria-label="工作目录"
          value={workspace}
          onChange={(event) => {
            const value = event.target.value;
            core.update((draft) => {
              draft.newSession.workspace = value;
            });
            void updateWorkspaceInput(core, value);
          }}
        />
        {suggestions.length > 0 ? (
          <div
            data-slot="workspace-suggestions"
            className="flex max-h-48 flex-col overflow-y-auto rounded-md border border-border bg-popover p-1"
          >
            {suggestions.map((item) => (
              <button
                key={item.path}
                type="button"
                data-slot="workspace-suggestion"
                data-path={item.path}
                onClick={() =>
                  pickWorkspace(item.path.endsWith("/") ? item.path : `${item.path}/`)
                }
                className="flex cursor-pointer items-center gap-1.5 rounded-sm px-2 py-1 text-left text-sm hover:bg-accent"
              >
                <Folder className="size-4 shrink-0 text-muted-foreground" />
                <span className="truncate">{item.name}</span>
              </button>
            ))}
          </div>
        ) : null}
        {showRecent ? (
          <div
            data-slot="recent-workspaces"
            className="flex flex-col rounded-md border border-border bg-popover p-1"
          >
            {recent.map((item) => (
              <button
                key={item.workspace}
                type="button"
                data-slot="recent-workspace"
                onClick={() => pickWorkspace(item.workspace)}
                className="cursor-pointer truncate rounded-sm px-2 py-1 text-left text-sm hover:bg-accent"
              >
                {item.workspace}
              </button>
            ))}
          </div>
        ) : null}
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

  const workflowForm =
    state.settings.orchestratorLoaded && state.settings.orchestrator === null ? (
      <div
        data-slot="orchestrator-missing"
        className="flex flex-col gap-3 rounded-md border border-destructive/50 bg-card p-3"
      >
        <p className="text-destructive">编排智能体未配置，无法创建工作流会话。</p>
        <Button
          data-slot="open-orchestrator-settings"
          variant="outline"
          onClick={() => openSettings(core, "orchestrator")}
        >
          前往设置
        </Button>
      </div>
    ) : (
      <div className="flex flex-col gap-3">
        <div className="flex flex-col gap-2">
          <Label>工作计划</Label>
          {state.settings.plans.length > 0 ? (
            <div className="flex flex-col gap-2">
              {state.settings.plans.map((item) => (
                <button
                  key={item.name}
                  type="button"
                  data-slot="plan-choice"
                  data-selected={selectedPlan === item.name ? "true" : "false"}
                  onClick={() =>
                    core.update((draft) => {
                      draft.newSession.plan = item.plan;
                      draft.newSession.selectedPlan = item.name;
                    })
                  }
                  className={cn(
                    "flex cursor-pointer flex-col gap-1 rounded-md border border-border bg-card p-2.5 text-left hover:bg-accent",
                    selectedPlan === item.name && "border-primary",
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
            onChange={(event) => {
              const value = event.target.value;
              core.update((draft) => {
                draft.newSession.plan = value;
                draft.newSession.selectedPlan = null;
              });
            }}
          />
        </div>
        <Button
          data-slot="create-workflow"
          disabled={plan.trim() === ""}
          onClick={() => void createWorkflow(core)}
        >
          创建会话
        </Button>
      </div>
    );

  return (
    <div
      data-slot="new-session-view"
      className="flex h-full flex-col items-center overflow-auto p-6"
    >
      <div className="flex w-full max-w-lg flex-col gap-4">
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
        {mode === "normal" ? normalForm : workflowForm}
      </div>
    </div>
  );
}
