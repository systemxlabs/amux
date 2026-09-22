// 项目管理设置（docs/PRD.md「项目管理设置」）。

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
import {
  createProject,
  deleteProject,
  setProjectOrder,
  updateProject,
} from "../../core/actions";
import { useCore, useCoreState } from "../../core/store";
import { truncate } from "../../lib/format";

/** 表单状态：`index === null` 表示新增。 */
type FormState = { index: number | null; name: string; description: string };

export function ProjectsSection() {
  const core = useCore();
  const state = useCoreState();
  const projects = state.settings.projects;
  const [form, setForm] = useState<FormState | null>(null);
  const [removing, setRemoving] = useState<string | null>(null);
  const [dragIndex, setDragIndex] = useState<number | null>(null);

  function submit(): void {
    if (form === null) return;
    const name = form.name.trim();
    if (name === "") return;
    // Server 要求名称唯一，且项目名称不支持修改
    if (
      form.index === null &&
      projects.some((item) => item.name === name)
    ) {
      core.failure("名称已存在");
      return;
    }
    if (form.index === null) {
      void createProject(core, name, form.description.trim());
    } else {
      void updateProject(core, projects[form.index].name, form.description.trim());
    }
    setForm(null);
  }

  function move(from: number, to: number): void {
    if (to < 0 || to >= projects.length || from === to) return;
    const next = [...projects];
    const [item] = next.splice(from, 1);
    next.splice(to, 0, item);
    void setProjectOrder(core, next.map((item) => item.name));
  }

  function drop(at: number): void {
    if (dragIndex === null || dragIndex === at) return;
    move(dragIndex, at);
    setDragIndex(null);
  }

  return (
    <section className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-medium">项目管理</h2>
        <Button
          variant="outline"
          size="icon"
          data-slot="project-add"
          aria-label="新增项目"
          onClick={() => setForm({ index: null, name: "", description: "" })}
        >
          +
        </Button>
      </div>
      {projects.length === 0 ? (
        <div className="text-xs text-muted-foreground">暂无项目</div>
      ) : (
        <div className="flex flex-col gap-3">
          {projects.map((project, index) => (
            <Card
              key={project.name}
              data-slot="project-card"
              data-project={project.name}
              draggable
              onDragStart={() => setDragIndex(index)}
              onDragOver={(event) => event.preventDefault()}
              onDrop={(event) => {
                event.preventDefault();
                drop(index);
              }}
              className="gap-2"
            >
              <CardHeader className="flex-row flex-wrap items-center justify-between gap-2">
                <CardTitle>{project.name}</CardTitle>
                <div className="flex flex-wrap items-center gap-1.5">
                  <Button
                    variant="outline"
                    size="sm"
                    data-slot="project-move-up"
                    disabled={index === 0}
                    onClick={() => move(index, index - 1)}
                  >
                    ↑
                  </Button>
                  <Button
                    variant="outline"
                    size="sm"
                    data-slot="project-move-down"
                    disabled={index + 1 >= projects.length}
                    onClick={() => move(index, index + 1)}
                  >
                    ↓
                  </Button>
                  <Button
                    variant="outline"
                    size="sm"
                    data-slot="project-edit"
                    onClick={() =>
                      setForm({
                        index,
                        name: project.name,
                        description: project.description,
                      })
                    }
                  >
                    编辑
                  </Button>
                  <Button
                    variant="destructive"
                    size="sm"
                    data-slot="project-delete"
                    onClick={() => setRemoving(project.name)}
                  >
                    删除
                  </Button>
                </div>
              </CardHeader>
              <CardContent className="text-xs text-muted-foreground">
                {truncate(project.description, 80)}
              </CardContent>
            </Card>
          ))}
        </div>
      )}
      <Dialog open={form !== null} onOpenChange={(next) => (!next ? setForm(null) : undefined)}>
        <DialogContent data-slot="project-form">
          <DialogHeader>
            <DialogTitle>{form?.index === null ? "新增项目" : "编辑项目"}</DialogTitle>
          </DialogHeader>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="project-form-name">名称</Label>
            <Input
              id="project-form-name"
              data-slot="project-form-name"
              disabled={form?.index !== null}
              value={form?.name ?? ""}
              onChange={(event) =>
                setForm(form === null ? form : { ...form, name: event.target.value })
              }
            />
          </div>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="project-form-desc">描述</Label>
            <Textarea
              id="project-form-desc"
              data-slot="project-form-desc"
              className="min-h-24"
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
            <Button data-slot="project-form-submit" onClick={submit}>
              保存
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <ConfirmDialog
        open={removing !== null}
        title="删除项目"
        description={
          removing === null ? undefined : `删除「${removing}」后其下会话将回到未归属，确认？`
        }
        onConfirm={() => {
          if (removing !== null) void deleteProject(core, removing);
          setRemoving(null);
        }}
        onCancel={() => setRemoving(null)}
      />
    </section>
  );
}
