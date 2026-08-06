import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { SessionRegistry, toMeta } from "./registry.js";
import { tmpDir } from "./testutil.js";

function entry(over: Partial<import("./registry.js").RegisteredSession> = {}): import("./registry.js").RegisteredSession {
  return {
    id: "s_1",
    harness: "codex",
    cwd: "/tmp/work",
    createdAt: 1000,
    lastEventAt: 1000,
    lastState: "idle",
    lastSeq: 7,
    closed: false,
    interrupted: false,
    ...over,
  };
}

describe("SessionRegistry", () => {
  it("保存/加载往返一致", () => {
    const file = join(tmpDir("amux-reg-"), "sessions.json");
    const r1 = new SessionRegistry(file);
    r1.upsert(entry({ id: "a", harness: "claude" }));
    r1.upsert(entry({ id: "b", closed: true }));
    r1.save();

    const r2 = new SessionRegistry(file);
    r2.load();
    expect(r2.has("a")).toBe(true);
    expect(r2.has("b")).toBe(true);
    expect(r2.get("a")).toMatchObject({ id: "a", harness: "claude", lastSeq: 7, closed: false });
    expect(r2.get("b")?.closed).toBe(true);
  });

  it("remove 后持久化", () => {
    const file = join(tmpDir("amux-reg-"), "sessions.json");
    const r1 = new SessionRegistry(file);
    r1.upsert(entry({ id: "a" }));
    r1.save();
    r1.remove("a");
    r1.save();

    const r2 = new SessionRegistry(file);
    r2.load();
    expect(r2.has("a")).toBe(false);
  });

  it("toMeta 映射 wire 形态", () => {
    expect(toMeta(entry({ interrupted: true }))).toEqual({
      id: "s_1",
      harness: "codex",
      cwd: "/tmp/work",
      model: undefined,
      state: "idle",
      interrupted: true,
      closed: false,
      createdAt: 1000,
      lastEventAt: 1000,
    });
  });
});
