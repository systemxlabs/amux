import { describe, expect, it } from "vitest";
import { loadConfig, requireToken } from "./config.js";

describe("loadConfig（token 指定方式）", () => {
  it("--token 生效", () => {
    const cfg = loadConfig(["--data-dir", "/tmp/x", "--token", "my-secret"]);
    expect(cfg.token).toBe("my-secret");
  });

  it("AMUX_TOKEN 环境变量生效", () => {
    const prev = process.env.AMUX_TOKEN;
    process.env.AMUX_TOKEN = "env-key";
    try {
      expect(loadConfig(["--data-dir", "/tmp/x"]).token).toBe("env-key");
    } finally {
      if (prev === undefined) delete process.env.AMUX_TOKEN;
      else process.env.AMUX_TOKEN = prev;
    }
  });

  it("未指定时 token 为 undefined（启动入口将拒绝启动）", () => {
    const prev = process.env.AMUX_TOKEN;
    delete process.env.AMUX_TOKEN;
    try {
      expect(loadConfig(["--data-dir", "/tmp/x"]).token).toBeUndefined();
    } finally {
      if (prev !== undefined) process.env.AMUX_TOKEN = prev;
    }
  });

  it("--api-key 不再生效（统一名称 token，不再支持别名）", () => {
    const prev = process.env.AMUX_TOKEN;
    delete process.env.AMUX_TOKEN;
    try {
      expect(loadConfig(["--data-dir", "/tmp/x", "--api-key", "old-alias"]).token).toBeUndefined();
    } finally {
      if (prev !== undefined) process.env.AMUX_TOKEN = prev;
    }
  });
});

describe("requireToken（每次启动由用户指定，不落盘不生成）", () => {
  it("指定 token 直接返回", () => {
    expect(requireToken("my-token")).toBe("my-token");
  });

  it("未指定（undefined / 空串）抛错——拒绝启动", () => {
    expect(() => requireToken(undefined)).toThrow(/token/);
    expect(() => requireToken("")).toThrow(/token/);
  });
});
