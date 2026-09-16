// 技能管理设置（docs/PRD.md「技能管理设置」、docs/DESIGN.md「技能操作」）。

import { useState } from "react";

import { ConfirmDialog } from "../../components/ConfirmDialog";
import { Button } from "../../components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "../../components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../../components/ui/dialog";
import { Input } from "../../components/ui/input";
import { Label } from "../../components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../components/ui/select";
import { runSkillAction, saveSkills } from "../../core/actions";
import { useCore, useCoreState } from "../../core/store";
import { truncate } from "../../lib/format";
import type { Skill } from "../../lib/types";

type SkillAction = "安装" | "更新" | "卸载";

const ACTIONS: { kind: SkillAction; slot: string }[] = [
  { kind: "安装", slot: "skill-install" },
  { kind: "更新", slot: "skill-update" },
  { kind: "卸载", slot: "skill-uninstall" },
];

/** 技能表单状态：`index === null` 表示新增。 */
type FormState = { index: number | null; name: string; description: string };

/** 技能操作弹窗状态（指定机器与 agent 后发起会话）。 */
type ActionState = {
  skill: Skill;
  action: SkillAction;
  machine: string;
  agent: string;
};

export function SkillsSection() {
  const core = useCore();
  const state = useCoreState();
  const skills = state.settings.skills;
  const [form, setForm] = useState<FormState | null>(null);
  const [removing, setRemoving] = useState<number | null>(null);
  const [action, setAction] = useState<ActionState | null>(null);

  function submit(): void {
    if (form === null) return;
    const name = form.name.trim();
    if (name === "") return;
    // Server 要求名称唯一
    if (skills.some((item, index) => item.name === name && index !== form.index)) {
      core.failure("名称已存在");
      return;
    }
    const next =
      form.index === null
        ? [...skills, { name, description: form.description }]
        : skills.map((item, index) =>
            index === form.index ? { name, description: form.description } : item,
          );
    void saveSkills(core, next);
    setForm(null);
  }

  function remove(index: number): void {
    setRemoving(null);
    void saveSkills(
      core,
      skills.filter((_, at) => at !== index),
    );
  }

  function run(): void {
    if (action === null) return;
    const current = action;
    setAction(null);
    void runSkillAction(core, current.skill, current.action, current.machine, current.agent);
  }

  const actionAgents =
    action === null
      ? []
      : (state.settings.agents.find((entry) => entry.machine === action.machine)?.agents ?? []);

  return (
    <section className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-medium">技能管理</h2>
        <Button
          variant="outline"
          size="icon"
          data-slot="skill-add"
          aria-label="新增技能"
          onClick={() => setForm({ index: null, name: "", description: "" })}
        >
          +
        </Button>
      </div>
      {skills.length === 0 ? (
        <div className="text-xs text-muted-foreground">暂无技能</div>
      ) : (
        <div className="flex flex-col gap-3">
          {skills.map((skill, index) => (
            <Card key={skill.name} data-slot="skill-card" className="gap-2">
              <CardHeader className="flex-row items-center justify-between">
                <CardTitle>{skill.name}</CardTitle>
                <div className="flex items-center gap-1.5">
                  {ACTIONS.map(({ kind, slot }) => (
                    <Button
                      key={kind}
                      variant={kind === "卸载" ? "destructive" : "outline"}
                      size="sm"
                      data-slot={slot}
                      onClick={() => setAction({ skill, action: kind, machine: "", agent: "" })}
                    >
                      {kind}
                    </Button>
                  ))}
                  <Button
                    variant="outline"
                    size="sm"
                    data-slot="skill-edit"
                    onClick={() =>
                      setForm({ index, name: skill.name, description: skill.description })
                    }
                  >
                    编辑
                  </Button>
                  <Button
                    variant="destructive"
                    size="sm"
                    data-slot="skill-delete"
                    onClick={() => setRemoving(index)}
                  >
                    删除
                  </Button>
                </div>
              </CardHeader>
              <CardContent className="text-xs text-muted-foreground">
                {truncate(skill.description, 80)}
              </CardContent>
            </Card>
          ))}
        </div>
      )}
      <Dialog open={form !== null} onOpenChange={(next) => (!next ? setForm(null) : undefined)}>
        <DialogContent data-slot="skill-form">
          <DialogHeader>
            <DialogTitle>{form?.index === null ? "新增技能" : "编辑技能"}</DialogTitle>
          </DialogHeader>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="skill-form-name">名称</Label>
            <Input
              id="skill-form-name"
              data-slot="skill-form-name"
              value={form?.name ?? ""}
              onChange={(event) =>
                setForm(form === null ? form : { ...form, name: event.target.value })
              }
            />
          </div>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="skill-form-description">描述</Label>
            <Input
              id="skill-form-description"
              data-slot="skill-form-description"
              value={form?.description ?? ""}
              onChange={(event) =>
                setForm(form === null ? form : { ...form, description: event.target.value })
              }
            />
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setForm(null)}>
              取消
            </Button>
            <Button data-slot="skill-form-submit" onClick={submit}>
              保存
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <Dialog open={action !== null} onOpenChange={(next) => (!next ? setAction(null) : undefined)}>
        <DialogContent data-slot="skill-action-dialog">
          <DialogHeader>
            <DialogTitle>
              {action === null ? "技能操作" : `${action.action}技能「${action.skill.name}」`}
            </DialogTitle>
          </DialogHeader>
          <div className="flex flex-col gap-1.5">
            <Label>指定机器</Label>
            <Select
              value={action?.machine ?? ""}
              onValueChange={(value) =>
                setAction(action === null ? action : { ...action, machine: value, agent: "" })
              }
            >
              <SelectTrigger className="w-full" data-slot="skill-action-machine">
                <SelectValue placeholder="选择机器" />
              </SelectTrigger>
              <SelectContent>
                {state.settings.machines.map((machine) => (
                  <SelectItem key={machine.name} value={machine.name}>
                    {machine.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div className="flex flex-col gap-1.5">
            <Label>指定 agent</Label>
            <Select
              value={action?.agent ?? ""}
              onValueChange={(value) =>
                setAction(action === null ? action : { ...action, agent: value })
              }
            >
              <SelectTrigger className="w-full" data-slot="skill-action-agent">
                <SelectValue placeholder="选择 agent" />
              </SelectTrigger>
              <SelectContent>
                {actionAgents.map((agent) => (
                  <SelectItem key={agent.name} value={agent.name} disabled={!agent.available}>
                    {agent.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setAction(null)}>
              取消
            </Button>
            <Button
              data-slot="skill-action-submit"
              disabled={action === null || action.machine === "" || action.agent === ""}
              onClick={run}
            >
              确认
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <ConfirmDialog
        open={removing !== null}
        title="删除技能"
        description={removing === null ? undefined : `确认删除「${skills[removing]?.name ?? ""}」？`}
        onConfirm={() => {
          if (removing !== null) remove(removing);
        }}
        onCancel={() => setRemoving(null)}
      />
    </section>
  );
}
