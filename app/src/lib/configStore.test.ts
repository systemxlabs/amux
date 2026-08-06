/**
 * GUI 配置存储：后端注入 + 旧版 localStorage 迁移 + 归一化。
 * 文件后端（Tauri Rust command）在测试环境不可用，统一注入内存后端。
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  addMachine,
  loadConfig,
  loadMachines,
  normalizeConfig,
  removeMachine,
  setConfigBackendForTests,
  type GuiConfig,
} from "./configStore.js";

function memoryBackend(initial: string | null = null) {
  let data = initial;
  return {
    load: async () => data,
    save: async (json: string) => {
      data = json;
    },
  };
}

function makeLocalStorage() {
  const m = new Map<string, string>();
  return {
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
    removeItem: (k: string) => void m.delete(k),
    clear: () => m.clear(),
    key: (i: number) => [...m.keys()][i] ?? null,
    get length() {
      return m.size;
    },
  };
}

beforeEach(() => {
  vi.stubGlobal("localStorage", makeLocalStorage());
});

afterEach(() => {
  setConfigBackendForTests(null);
  vi.unstubAllGlobals();
});

describe("loadConfig：默认与归一化", () => {
  it("空后端无旧数据 → 默认配置", async () => {
    setConfigBackendForTests(memoryBackend());
    const cfg = await loadConfig();
    expect(cfg.machines).toEqual([]);
    expect(cfg.buttons).toEqual([]);
    expect(cfg.notify).toEqual({ workEnded: true, onError: true, longIdleSeconds: 300 });
  });

  it("损坏 JSON → 回退默认，不抛错", async () => {
    setConfigBackendForTests(memoryBackend("{not-json"));
    const cfg = await loadConfig();
    expect(cfg.machines).toEqual([]);
  });

  it("normalizeConfig：非法字段回退默认，合法字段保留", () => {
    const cfg = normalizeConfig({
      machines: [{ id: "m1", name: "本机", url: "ws://127.0.0.1:34567", token: "t", defaultModel: "gpt" }, { bad: true }],
      buttons: [{ id: "x", label: "X" }],
      notify: { workEnded: false, onError: "yes" },
    });
    expect(cfg.machines).toHaveLength(1);
    expect(cfg.machines[0]).toMatchObject({ id: "m1", defaultModel: "gpt" });
    expect(cfg.buttons).toHaveLength(1);
    expect(cfg.notify).toEqual({ workEnded: false, onError: true, longIdleSeconds: 300 });
  });
});

describe("旧版 localStorage 迁移", () => {
  it("首次运行：后端为空时读取旧键、写入新存储、清理旧键", async () => {
    const b = memoryBackend();
    setConfigBackendForTests(b);
    localStorage.setItem("amux.machines.v1", JSON.stringify([{ id: "m1", name: "本机", url: "ws://x", token: "t" }]));
    localStorage.setItem("amux.notify.v1", JSON.stringify({ workEnded: false, onError: true, longIdleSeconds: 60 }));

    const cfg = await loadConfig();
    expect(cfg.machines).toHaveLength(1);
    expect(cfg.notify.longIdleSeconds).toBe(60);
    // 已写入新后端（可再读回）
    expect((await loadConfig()).machines[0].id).toBe("m1");
    // 旧键已清理
    expect(localStorage.getItem("amux.machines.v1")).toBeNull();
    expect(localStorage.getItem("amux.notify.v1")).toBeNull();
  });

  it("后端已有配置时不迁移（后端优先）", async () => {
    const existing: GuiConfig = { version: 1, machines: [{ id: "m9", name: "新", url: "ws://y", token: "t2" }], buttons: [], notify: { workEnded: true, onError: true, longIdleSeconds: 300 } };
    setConfigBackendForTests(memoryBackend(JSON.stringify(existing)));
    localStorage.setItem("amux.machines.v1", JSON.stringify([{ id: "legacy", name: "旧", url: "ws://z", token: "t3" }]));

    const cfg = await loadConfig();
    expect(cfg.machines[0].id).toBe("m9");
    expect(localStorage.getItem("amux.machines.v1")).not.toBeNull(); // 旧键保留
  });
});

describe("机器增删往返", () => {
  it("addMachine → removeMachine 经后端持久化", async () => {
    const b = memoryBackend();
    setConfigBackendForTests(b);

    const afterAdd = await addMachine({ name: "远程", url: "ws://1.2.3.4:34567", token: "tok", defaultModel: "m" });
    expect(afterAdd).toHaveLength(1);
    expect(afterAdd[0].id).toBeTruthy();

    const loaded = await loadMachines();
    expect(loaded).toEqual(afterAdd);

    const afterRemove = await removeMachine(afterAdd[0].id);
    expect(afterRemove).toEqual([]);
    expect(await loadMachines()).toEqual([]);
  });
});
