/**
 * Harness 注册表：名字 → ahal Driver 工厂 + 轻量可用性探测。
 * server 与 harness 的唯一接口层是 AHAL（docs/AHAL.md）；本文件只负责装配与探测。
 */

import { spawnSync } from "node:child_process";
import type { Driver } from "ahal";
import { createClaudeDriver } from "ahal-claude";
import { createCodexDriver } from "ahal-codex";
import { createKimiDriver } from "ahal-kimi";
import type { HarnessInfo, HarnessName } from "shared";

export type HarnessProbe = () => boolean;

export interface HarnessSpec {
  name: HarnessName;
  createDriver: () => Driver;
  probe: HarnessProbe;
  defaultModel?: string;
}

export class HarnessRegistry {
  private readonly specs = new Map<string, HarnessSpec>();

  constructor(specs: HarnessSpec[]) {
    for (const s of specs) this.specs.set(s.name, s);
  }

  has(name: string): boolean {
    return this.specs.has(name);
  }

  names(): HarnessName[] {
    return [...this.specs.keys()] as HarnessName[];
  }

  createDriver(name: HarnessName): Driver {
    const spec = this.specs.get(name);
    if (!spec) throw new Error(`未知 harness: ${name}`);
    return spec.createDriver();
  }

  info(): HarnessInfo[] {
    return this.names().map((name) => {
      const spec = this.specs.get(name)!;
      let available = false;
      try {
        available = spec.probe();
      } catch {
        available = false;
      }
      return { name, available, ...(spec.defaultModel ? { defaultModel: spec.defaultModel } : {}) };
    });
  }
}

/** 探测：以 `binary --version` 退出码判定二进制可用。 */
export function commandProbe(binary: string): HarnessProbe {
  return () => {
    try {
      const r = spawnSync(binary, ["--version"], { stdio: "ignore", timeout: 10_000 });
      return r.status === 0;
    } catch {
      return false;
    }
  };
}

/** 默认三个 harness 的装配；config 可为各 harness 指定默认模型。 */
export function defaultHarnessSpecs(config?: { harnesses?: Record<string, { defaultModel?: string }> }): HarnessSpec[] {
  const hc = config?.harnesses ?? {};
  const model = (name: string): string | undefined => hc[name]?.defaultModel;
  return [
    {
      name: "codex",
      createDriver: () => createCodexDriver(),
      probe: commandProbe(process.env.CODEX_BINARY ?? "codex"),
      ...(model("codex") ? { defaultModel: model("codex") } : {}),
    },
    {
      name: "claude",
      createDriver: () => createClaudeDriver(),
      probe: commandProbe("claude"),
      ...(model("claude") ? { defaultModel: model("claude") } : {}),
    },
    {
      name: "kimi",
      createDriver: () => createKimiDriver(),
      probe: commandProbe("kimi"),
      ...(model("kimi") ? { defaultModel: model("kimi") } : {}),
    },
  ];
}
