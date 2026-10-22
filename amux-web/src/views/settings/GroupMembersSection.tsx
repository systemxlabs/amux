// 群聊预设成员设置（docs/PRD.md「群聊设置」）。

import { useState } from "react";

import { ConfirmDialog } from "../../components/ConfirmDialog";
import { GroupMemberFields } from "../../components/GroupMembersEditor";
import { Button } from "../../components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "../../components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../../components/ui/dialog";
import { saveGroupMembers } from "../../core/actions";
import { useCore, useCoreState } from "../../core/store";
import type { GroupMemberTemplate } from "../../lib/types";

type FormState = { index: number | null; member: GroupMemberTemplate };

function emptyMember(): GroupMemberTemplate {
  return {
    name: "",
    description: "",
    machine: "",
    agent: "",
    workspace: "",
    useWorktree: false,
  };
}

export function GroupMembersSection() {
  const core = useCore();
  const state = useCoreState();
  const members = state.settings.groupMembers;
  const [form, setForm] = useState<FormState | null>(null);
  const [removing, setRemoving] = useState<number | null>(null);

  function submit(): void {
    if (form === null) return;
    const member = {
      ...form.member,
      name: form.member.name.trim(),
      description: form.member.description.trim(),
      workspace: form.member.workspace.trim(),
    };
    if (
      member.name === "" ||
      member.description === "" ||
      member.machine === "" ||
      member.agent === "" ||
      member.workspace === ""
    ) {
      core.failure("请填写成员名称、角色描述、机器、agent 和工作目录");
      return;
    }
    if (member.useWorktree && member.worktreeBase === undefined) {
      core.failure("开启 worktree 时必须选择基分支");
      return;
    }
    const next =
      form.index === null
        ? [...members, member]
        : members.map((item, index) => (index === form.index ? member : item));
    void saveGroupMembers(core, next);
    setForm(null);
  }

  function remove(index: number): void {
    setRemoving(null);
    void saveGroupMembers(
      core,
      members.filter((_, current) => current !== index),
    );
  }

  return (
    <section className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-medium">群聊预设成员</h2>
        <Button
          variant="outline"
          size="icon"
          data-slot="group-member-add"
          aria-label="新增群聊预设成员"
          onClick={() => setForm({ index: null, member: emptyMember() })}
        >
          +
        </Button>
      </div>
      {members.length === 0 ? (
        <div className="text-xs text-muted-foreground">暂无群聊预设成员</div>
      ) : (
        <div className="flex flex-col gap-3">
          {members.map((member, index) => (
            <Card key={index} data-slot="group-member-card" className="gap-2">
              <CardHeader className="flex-row flex-wrap items-center justify-between gap-2">
                <CardTitle>{member.name}</CardTitle>
                <div className="flex flex-wrap items-center gap-1.5">
                  <Button
                    variant="outline"
                    size="sm"
                    data-slot="group-member-edit"
                    onClick={() =>
                      setForm({
                        index,
                        member: { ...member },
                      })
                    }
                  >
                    编辑
                  </Button>
                  <Button
                    variant="destructive"
                    size="sm"
                    data-slot="group-member-delete"
                    onClick={() => setRemoving(index)}
                  >
                    删除
                  </Button>
                </div>
              </CardHeader>
              <CardContent className="flex flex-col gap-1 text-xs text-muted-foreground">
                <span className="whitespace-pre-wrap break-words">{member.description}</span>
                <span>
                  {member.agent}@{member.machine} · {member.workspace}
                </span>
              </CardContent>
            </Card>
          ))}
        </div>
      )}
      <Dialog open={form !== null} onOpenChange={(next) => (!next ? setForm(null) : undefined)}>
        <DialogContent data-slot="group-member-form">
          <DialogHeader>
            <DialogTitle>
              {form?.index === null ? "新增群聊预设成员" : "编辑群聊预设成员"}
            </DialogTitle>
          </DialogHeader>
          {form !== null ? (
            <GroupMemberFields
              core={core}
              member={form.member}
              onChange={(member) => setForm({ ...form, member })}
              slotPrefix="group-member-form"
            />
          ) : null}
          <DialogFooter>
            <Button variant="outline" onClick={() => setForm(null)}>
              取消
            </Button>
            <Button data-slot="group-member-form-submit" onClick={submit}>
              保存
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <ConfirmDialog
        open={removing !== null}
        title="删除群聊预设成员"
        description={
          removing === null ? undefined : `确认删除「${members[removing]?.name ?? ""}」？`
        }
        onConfirm={() => {
          if (removing !== null) remove(removing);
        }}
        onCancel={() => setRemoving(null)}
      />
    </section>
  );
}
