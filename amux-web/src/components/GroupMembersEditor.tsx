// 群聊成员编辑器：新建会话、自动化和群聊预设设置共用同一套成员字段与校验。

import { Plus, Trash2 } from "lucide-react";

import type { Core, CoreState } from "../core/core";
import { useCoreState } from "../core/store";
import type { GroupMemberTemplate } from "../lib/types";
import { cn } from "../lib/utils";
import { WorkspaceField, WorktreeBranchField } from "./SessionCreationFields";
import { Button } from "./ui/button";
import { Input } from "./ui/input";
import { Label } from "./ui/label";
import { Switch } from "./ui/switch";
import { Textarea } from "./ui/textarea";

/**
 * 成员能否用于创建群聊。预设列表允许名称重复，但同一群聊内必须唯一。
 */
export function groupMembersError(
  members: readonly GroupMemberTemplate[],
  agents: CoreState["settings"]["agents"],
): string | null {
  if (members.length === 0) return "请至少添加一个群聊成员";
  const names = new Set<string>();
  for (const member of members) {
    const name = member.name.trim();
    if (
      name === "" ||
      member.description.trim() === "" ||
      member.machine === "" ||
      member.agent === "" ||
      member.workspace.trim() === ""
    ) {
      return "请填写成员名称、角色描述、机器、agent 和工作目录";
    }
    if (names.has(name)) return "群聊内的成员名称不能重复";
    names.add(name);
    const agent = agents
      .find((item) => item.machine === member.machine)
      ?.agents.find((item) => item.name === member.agent);
    if (agent === undefined || !agent.available) {
      return `成员「${name}」未选择可用 agent`;
    }
    if (member.useWorktree && member.worktreeBase === undefined) {
      return `成员「${name}」开启 worktree 时必须选择基分支`;
    }
  }
  return null;
}

export function GroupMemberFields({
  core,
  member,
  onChange,
  slotPrefix = "group-member",
}: {
  core: Core;
  member: GroupMemberTemplate;
  onChange: (member: GroupMemberTemplate) => void;
  slotPrefix?: string;
}) {
  const state = useCoreState();
  const agents =
    state.settings.agents.find((item) => item.machine === member.machine)?.agents ?? [];

  return (
    <div className="grid gap-3">
      <div className="grid gap-1.5">
        <Label>成员名称</Label>
        <Input
          data-slot={`${slotPrefix}-name`}
          value={member.name}
          onChange={(event) => onChange({ ...member, name: event.target.value })}
        />
      </div>
      <div className="grid gap-1.5">
        <Label>角色描述</Label>
        <Textarea
          data-slot={`${slotPrefix}-description`}
          className="min-h-20"
          value={member.description}
          onChange={(event) => onChange({ ...member, description: event.target.value })}
        />
      </div>
      <div className="grid gap-1.5">
        <Label>机器</Label>
        <div className="flex flex-wrap gap-1">
          {state.settings.machines.map((machine) => (
            <button
              key={machine.name}
              type="button"
              data-slot={`${slotPrefix}-machine`}
              aria-pressed={member.machine === machine.name}
              onClick={() =>
                onChange({ ...member, machine: machine.name, agent: "" })
              }
              className={cn(
                "min-h-9 rounded-sm border border-border bg-card px-2.5 py-1 text-xs",
                member.machine === machine.name
                  ? "border-primary bg-accent"
                  : "cursor-pointer hover:bg-accent",
              )}
            >
              {machine.name}
            </button>
          ))}
        </div>
      </div>
      <div className="grid gap-1.5">
        <Label>智能体</Label>
        {agents.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            {member.machine === "" ? "请选择机器" : "该机器未发现智能体"}
          </p>
        ) : (
          <div className="flex flex-wrap gap-1">
            {agents.map((agent) => (
              <button
                key={agent.name}
                type="button"
                data-slot={`${slotPrefix}-agent`}
                data-available={agent.available ? "true" : "false"}
                aria-pressed={member.agent === agent.name}
                disabled={!agent.available}
                onClick={() => onChange({ ...member, agent: agent.name })}
                className={cn(
                  "min-h-9 rounded-sm border border-border bg-card px-2.5 py-1 text-xs",
                  member.agent === agent.name && "border-primary bg-accent",
                  agent.available
                    ? "cursor-pointer hover:bg-accent"
                    : "cursor-not-allowed opacity-50",
                )}
              >
                {agent.name}
              </button>
            ))}
          </div>
        )}
      </div>
      <div className="grid gap-1.5">
        <Label>工作目录</Label>
        <WorkspaceField
          core={core}
          machine={member.machine}
          value={member.workspace}
          onChange={(workspace) => onChange({ ...member, workspace })}
          slotPrefix={`${slotPrefix}-workspace`}
        />
      </div>
      <div className="flex items-center justify-between gap-3">
        <Label>使用 worktree</Label>
        <Switch
          data-slot={`${slotPrefix}-worktree`}
          aria-label="使用 worktree"
          checked={member.useWorktree}
          onCheckedChange={(useWorktree) =>
            onChange({
              ...member,
              useWorktree,
              worktreeBase: useWorktree ? member.worktreeBase : undefined,
            })
          }
        />
      </div>
      {member.useWorktree ? (
        <WorktreeBranchField
          core={core}
          machine={member.machine}
          workspace={member.workspace}
          value={member.worktreeBase ?? null}
          onChange={(worktreeBase) =>
            onChange({ ...member, worktreeBase: worktreeBase ?? undefined })
          }
        />
      ) : null}
    </div>
  );
}

export function GroupMembersEditor({
  core,
  members,
  onChange,
  slotPrefix = "group-member",
}: {
  core: Core;
  members: GroupMemberTemplate[];
  onChange: (members: GroupMemberTemplate[]) => void;
  slotPrefix?: string;
}) {
  const state = useCoreState();
  const duplicateNames = new Set(
    members
      .map((member) => member.name.trim())
      .filter(
        (name, index, names) =>
          name !== "" && names.findIndex((candidate) => candidate === name) !== index,
      ),
  );

  const update = (index: number, member: GroupMemberTemplate): void => {
    onChange(members.map((item, current) => (current === index ? member : item)));
  };

  return (
    <div className="grid gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <Label>群聊成员</Label>
        <Button
          type="button"
          variant="outline"
          size="sm"
          data-slot={`${slotPrefix}-add`}
          onClick={() =>
            onChange([
              ...members,
              {
                name: "",
                description: "",
                machine: "",
                agent: "",
                workspace: "",
                useWorktree: false,
              },
            ])
          }
        >
          <Plus className="size-3.5" />
          自定义成员
        </Button>
      </div>
      {state.settings.groupMembers.length > 0 ? (
        <div className="flex flex-col gap-1.5">
          <span className="text-xs text-muted-foreground">从群聊预设成员添加</span>
          <div className="flex flex-wrap gap-1">
            {state.settings.groupMembers.map((template, index) => (
              <button
                key={`${template.name}:${index}`}
                type="button"
                data-slot={`${slotPrefix}-preset`}
                onClick={() =>
                  onChange([
                    ...members,
                    {
                      ...template,
                      worktreeBase:
                        template.worktreeBase === undefined
                          ? undefined
                          : { ...template.worktreeBase },
                    },
                  ])
                }
                className="cursor-pointer rounded-sm border border-border bg-card px-2 py-1 text-xs hover:bg-accent"
              >
                {template.name}
              </button>
            ))}
          </div>
        </div>
      ) : null}
      {members.length === 0 ? (
        <p className="rounded-md border border-dashed border-border p-3 text-center text-xs text-muted-foreground">
          尚未添加群聊成员
        </p>
      ) : null}
      {members.map((member, index) => (
        <div
          key={index}
          data-slot={`${slotPrefix}-card`}
          className="grid gap-3 rounded-md border border-border p-3"
        >
          <div className="flex items-center justify-between gap-2">
            <span className="text-xs font-medium">
              {member.name.trim() === "" ? `成员 ${index + 1}` : member.name}
            </span>
            <Button
              type="button"
              variant="ghost"
              size="icon"
              aria-label="删除成员"
              title="删除成员"
              onClick={() => onChange(members.filter((_, current) => current !== index))}
            >
              <Trash2 />
            </Button>
          </div>
          {duplicateNames.has(member.name.trim()) ? (
            <p className="text-xs text-destructive">群聊内的成员名称不能重复</p>
          ) : null}
          <GroupMemberFields
            core={core}
            member={member}
            onChange={(next) => update(index, next)}
            slotPrefix={`${slotPrefix}-${index}`}
          />
        </div>
      ))}
    </div>
  );
}
