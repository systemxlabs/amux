// 工作流计划设置（docs/PRD.md「工作流计划设置」）。

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
import { Textarea } from "../../components/ui/textarea";
import { savePlans } from "../../core/actions";
import { useCore, useCoreState } from "../../core/store";
import { truncate } from "../../lib/format";

/** 表单状态：`index === null` 表示新增。 */
type FormState = { index: number | null; name: string; plan: string };

export function WorkflowPlansSection() {
  const core = useCore();
  const state = useCoreState();
  const plans = state.settings.plans;
  const [form, setForm] = useState<FormState | null>(null);
  const [removing, setRemoving] = useState<number | null>(null);

  function submit(): void {
    if (form === null) return;
    const name = form.name.trim();
    if (name === "") return;
    // Server 要求名称唯一
    if (plans.some((item, index) => item.name === name && index !== form.index)) {
      core.failure("名称已存在");
      return;
    }
    const next =
      form.index === null
        ? [...plans, { name, plan: form.plan }]
        : plans.map((item, index) => (index === form.index ? { name, plan: form.plan } : item));
    void savePlans(core, next);
    setForm(null);
  }

  function remove(index: number): void {
    setRemoving(null);
    void savePlans(
      core,
      plans.filter((_, at) => at !== index),
    );
  }

  return (
    <section className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-medium">工作流计划</h2>
        <Button
          variant="outline"
          size="icon"
          data-slot="plan-add"
          aria-label="新增工作流计划"
          onClick={() => setForm({ index: null, name: "", plan: "" })}
        >
          +
        </Button>
      </div>
      {plans.length === 0 ? (
        <div className="text-xs text-muted-foreground">暂无工作流计划</div>
      ) : (
        <div className="flex flex-col gap-3">
          {plans.map((plan, index) => (
            <Card key={plan.name} data-slot="plan-card" className="gap-2">
              <CardHeader className="flex-row flex-wrap items-center justify-between gap-2">
                <CardTitle>{plan.name}</CardTitle>
                <div className="flex flex-wrap items-center gap-1.5">
                  <Button
                    variant="outline"
                    size="sm"
                    data-slot="plan-edit"
                    onClick={() => setForm({ index, name: plan.name, plan: plan.plan })}
                  >
                    编辑
                  </Button>
                  <Button
                    variant="destructive"
                    size="sm"
                    data-slot="plan-delete"
                    onClick={() => setRemoving(index)}
                  >
                    删除
                  </Button>
                </div>
              </CardHeader>
              <CardContent className="text-xs text-muted-foreground">
                {truncate(plan.plan, 80)}
              </CardContent>
            </Card>
          ))}
        </div>
      )}
      <Dialog open={form !== null} onOpenChange={(next) => (!next ? setForm(null) : undefined)}>
        <DialogContent data-slot="plan-form">
          <DialogHeader>
            <DialogTitle>{form?.index === null ? "新增工作流计划" : "编辑工作流计划"}</DialogTitle>
          </DialogHeader>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="plan-form-name">名称</Label>
            <Input
              id="plan-form-name"
              data-slot="plan-form-name"
              value={form?.name ?? ""}
              onChange={(event) =>
                setForm(form === null ? form : { ...form, name: event.target.value })
              }
            />
          </div>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="plan-form-plan">计划</Label>
            <Textarea
              id="plan-form-plan"
              data-slot="plan-form-plan"
              className="min-h-32"
              value={form?.plan ?? ""}
              onChange={(event) =>
                setForm(form === null ? form : { ...form, plan: event.target.value })
              }
            />
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setForm(null)}>
              取消
            </Button>
            <Button data-slot="plan-form-submit" onClick={submit}>
              保存
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <ConfirmDialog
        open={removing !== null}
        title="删除工作流计划"
        description={removing === null ? undefined : `确认删除「${plans[removing]?.name ?? ""}」？`}
        onConfirm={() => {
          if (removing !== null) remove(removing);
        }}
        onCancel={() => setRemoving(null)}
      />
    </section>
  );
}
