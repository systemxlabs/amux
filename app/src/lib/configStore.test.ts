/**
 * GUI 配置存储：后端注入 + 归一化 + 机器增删往返。
 * 文件后端（Tauri Rust command）在测试环境不可用，统一注入内存后端。
 */

import { afterEach, describe, expect, it } from "vitest";
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

afterEach(() => {
  setConfigBackendForTests(null);
});

describe("loadConfig：默认与归一化", () => {
  it("空后端 → 默认配置", async () => {
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

  it("后端已有配置时原样读回", async () => {
    const existing: GuiConfig = {
      version: 1,
      machines: [{ id: "m9", name: "新", url: "ws://y", token: "t2" }],
      buttons: [],
      notify: { workEnded: true, onError: true, longIdleSeconds: 300 },
    };
    setConfigBackendForTests(memoryBackend(JSON.stringify(existing)));
    const cfg = await loadConfig();
    expect(cfg.machines[0].id).toBe("m9");
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
