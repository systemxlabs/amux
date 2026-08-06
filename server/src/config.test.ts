import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { loadConfig, loadOrCreateToken } from "./config.js";
import { tmpDir } from "./testutil.js";

describe("loadConfig（token 指定方式）", () => {
  it("--token 生效", () => {
    const cfg = loadConfig(["--data-dir", "/tmp/x", "--token", "my-secret"]);
    expect(cfg.token).toBe("my-secret");
  });

  it("--api-key 别名生效（参考 raft-daemon 风格）", () => {
    const cfg = loadConfig(["--data-dir", "/tmp/x", "--api-key", "raft-style-key"]);
    expect(cfg.token).toBe("raft-style-key");
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

  it("未指定时 token 为 undefined（走文件或自动生成）", () => {
    const prev = process.env.AMUX_TOKEN;
    delete process.env.AMUX_TOKEN;
    try {
      expect(loadConfig(["--data-dir", "/tmp/x"]).token).toBeUndefined();
    } finally {
      if (prev !== undefined) process.env.AMUX_TOKEN = prev;
    }
  });
});

describe("loadOrCreateToken", () => {
  it("指定 token：直接使用并写入文件（重启沿用）", () => {
    const dir = tmpDir("amux-cfg-");
    const r = loadOrCreateToken(dir, "指定token");
    expect(r).toEqual({ token: "指定token", newlyCreated: false });
    expect(readFileSync(join(dir, "token"), "utf8").trim()).toBe("指定token");
  });

  it("未指定且文件已有：复用文件 token", () => {
    const dir = tmpDir("amux-cfg-");
    loadOrCreateToken(dir, "first");
    const r = loadOrCreateToken(dir, undefined);
    expect(r.token).toBe("first");
    expect(r.newlyCreated).toBe(false);
  });

  it("都没有：随机生成并持久化（仅展示一次）", () => {
    const dir = tmpDir("amux-cfg-");
    const r = loadOrCreateToken(dir, undefined);
    expect(r.newlyCreated).toBe(true);
    expect(r.token.length).toBeGreaterThan(16);
    expect(existsSync(join(dir, "token"))).toBe(true);
    expect(readFileSync(join(dir, "token"), "utf8").trim()).toBe(r.token);
  });

  it("再次启动不再生成新 token", () => {
    const dir = tmpDir("amux-cfg-");
    const first = loadOrCreateToken(dir, undefined);
    const second = loadOrCreateToken(dir, undefined);
    expect(second.token).toBe(first.token);
  });
});
