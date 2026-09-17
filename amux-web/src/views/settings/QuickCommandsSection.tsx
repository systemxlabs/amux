// 快捷指令设置（docs/PRD.md「快捷指令设置」）。

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
import { Textarea } from "../../components/ui/textarea";
import { Label } from "../../components/ui/label";
import { saveQuickCommands } from "../../core/actions";
import { useCore, useCoreState } from "../../core/store";
import { truncate } from "../../lib/format";

/** 表单状态：`index === null` 表示新增。 */
type FormState = { index: number | null; name: string; prompt: string };

export function QuickCommandsSection() {
  const core = useCore();
  const state = useCoreState();
  const commands = state.settings.quickCommands;
  const [form, setForm] = useState<FormState | null>(null);
  const [removing, setRemoving] = useState<number | null>(null);

  function submit(): void {
    if (form === null) return;
    const name = form.name.trim();
    if (name === "") return;
    // Server 要求名称唯一
    if (commands.some((item, index) => item.name === name && index !== form.index)) {
      core.failure("名称已存在");
      return;
    }
    const next =
      form.index === null
        ? [...commands, { name, prompt: form.prompt }]
        : commands.map((item, index) =>
            index === form.index ? { name, prompt: form.prompt } : item,
          );
    void saveQuickCommands(core, next);
    setForm(null);
  }

  function remove(index: number): void {
    setRemoving(null);
    void saveQuickCommands(
      core,
      commands.filter((_, at) => at !== index),
    );
  }

  return (
    <section className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-medium">快捷指令</h2>
        <Button
          variant="outline"
          size="icon"
          data-slot="quick-command-add"
          aria-label="新增快捷指令"
          onClick={() => setForm({ index: null, name: "", prompt: "" })}
        >
          +
        </Button>
      </div>
      {commands.length === 0 ? (
        <div className="text-xs text-muted-foreground">暂无快捷指令</div>
      ) : (
        <div className="flex flex-col gap-3">
          {commands.map((command, index) => (
            <Card key={command.name} data-slot="quick-command-card" className="gap-2">
              <CardHeader className="flex-row flex-wrap items-center justify-between gap-2">
                <CardTitle>{command.name}</CardTitle>
                <div className="flex flex-wrap items-center gap-1.5">
                  <Button
                    variant="outline"
                    size="sm"
                    data-slot="quick-command-edit"
                    onClick={() =>
                      setForm({ index, name: command.name, prompt: command.prompt })
                    }
                  >
                    编辑
                  </Button>
                  <Button
                    variant="destructive"
                    size="sm"
                    data-slot="quick-command-delete"
                    onClick={() => setRemoving(index)}
                  >
                    删除
                  </Button>
                </div>
              </CardHeader>
              <CardContent className="text-xs text-muted-foreground">
                {truncate(command.prompt, 80)}
              </CardContent>
            </Card>
          ))}
        </div>
      )}
      <Dialog open={form !== null} onOpenChange={(next) => (!next ? setForm(null) : undefined)}>
        <DialogContent data-slot="quick-command-form">
          <DialogHeader>
            <DialogTitle>{form?.index === null ? "新增快捷指令" : "编辑快捷指令"}</DialogTitle>
          </DialogHeader>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="quick-command-form-name">名称</Label>
            <Input
              id="quick-command-form-name"
              data-slot="quick-command-form-name"
              value={form?.name ?? ""}
              onChange={(event) =>
                setForm(form === null ? form : { ...form, name: event.target.value })
              }
            />
          </div>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="quick-command-form-prompt">提示词</Label>
            <Textarea
              id="quick-command-form-prompt"
              data-slot="quick-command-form-prompt"
              className="min-h-32"
              value={form?.prompt ?? ""}
              onChange={(event) =>
                setForm(form === null ? form : { ...form, prompt: event.target.value })
              }
            />
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setForm(null)}>
              取消
            </Button>
            <Button data-slot="quick-command-form-submit" onClick={submit}>
              保存
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <ConfirmDialog
        open={removing !== null}
        title="删除快捷指令"
        description={
          removing === null ? undefined : `确认删除「${commands[removing]?.name ?? ""}」？`
        }
        onConfirm={() => {
          if (removing !== null) remove(removing);
        }}
        onCancel={() => setRemoving(null)}
      />
    </section>
  );
}
