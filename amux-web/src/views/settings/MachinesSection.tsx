// 机器管理设置（docs/PRD.md「机器管理设置」）。

import { useState } from "react";

import { ConfirmDialog } from "../../components/ConfirmDialog";
import { Button } from "../../components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "../../components/ui/card";
import { rediscover, restartAgent } from "../../core/actions";
import { useCore, useCoreState } from "../../core/store";

/** 待二次确认的机器操作。 */
type Pending =
  | { kind: "rediscover"; machine: string }
  | { kind: "restart"; machine: string; agent: string };

export function MachinesSection() {
  const core = useCore();
  const state = useCoreState();
  const [pending, setPending] = useState<Pending | null>(null);
  const machines = state.settings.machines;

  function confirm(): void {
    const action = pending;
    setPending(null);
    if (action === null) return;
    if (action.kind === "rediscover") {
      void rediscover(core, action.machine);
    } else {
      void restartAgent(core, action.machine, action.agent);
    }
  }

  return (
    <section className="flex flex-col gap-4">
      <h2 className="text-sm font-medium">机器管理</h2>
      {machines.length === 0 ? (
        <div className="text-xs text-muted-foreground">暂无机器</div>
      ) : (
        <div className="flex flex-col gap-3">
          {machines.map((machine) => {
            const agents =
              state.settings.agents.find((entry) => entry.machine === machine.name)?.agents ?? [];
            return (
              <Card key={machine.name} data-slot="machine-card" className="gap-3">
                <CardHeader className="flex-row flex-wrap items-center justify-between gap-2">
                  <CardTitle>{machine.name}</CardTitle>
                  <Button
                    variant="outline"
                    size="sm"
                    data-slot="machine-rediscover"
                    onClick={() => setPending({ kind: "rediscover", machine: machine.name })}
                  >
                    重新发现
                  </Button>
                </CardHeader>
                <CardContent className="flex flex-col gap-3">
                  <dl className="grid w-fit grid-cols-[5rem_1fr] gap-x-3 gap-y-1 text-xs">
                    <dt className="text-muted-foreground">操作系统</dt>
                    <dd>{machine.os}</dd>
                    <dt className="text-muted-foreground">架构</dt>
                    <dd>{machine.arch}</dd>
                    <dt className="text-muted-foreground">主机名</dt>
                    <dd>{machine.hostname}</dd>
                    <dt className="text-muted-foreground">临时目录</dt>
                    <dd className="break-all">{machine.tempDir}</dd>
                    <dt className="text-muted-foreground">版本</dt>
                    <dd>{machine.version}</dd>
                  </dl>
                  <div className="flex flex-col gap-1">
                    {agents.map((agent) => (
                      <div
                        key={agent.name}
                        data-slot="agent-row"
                        className="flex items-center justify-between gap-2 rounded-md border border-border px-2 py-1 text-xs"
                      >
                        <span>{agent.name}</span>
                        <div className="flex items-center gap-2">
                          <span className="text-muted-foreground">
                            {agent.available ? "可用" : "不可用"}
                          </span>
                          <Button
                            variant="outline"
                            size="sm"
                            data-slot="agent-restart"
                            onClick={() =>
                              setPending({
                                kind: "restart",
                                machine: machine.name,
                                agent: agent.name,
                              })
                            }
                          >
                            重启
                          </Button>
                        </div>
                      </div>
                    ))}
                  </div>
                </CardContent>
              </Card>
            );
          })}
        </div>
      )}
      <ConfirmDialog
        open={pending !== null}
        title={pending?.kind === "restart" ? "重启 agent" : "重新发现机器"}
        description={
          pending === null
            ? undefined
            : pending.kind === "restart"
              ? `确认重启「${pending.agent}@${pending.machine}」？`
              : `确认重新发现机器「${pending.machine}」上的 agents？`
        }
        onConfirm={confirm}
        onCancel={() => setPending(null)}
      />
    </section>
  );
}
