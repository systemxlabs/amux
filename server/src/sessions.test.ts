import { join } from "node:path";
import { HarnessUnavailableError, SessionClosedError, SessionNotFoundError, type SessionState } from "ahal";
import { describe, expect, it } from "vitest";
import { HistoryStore } from "./history.js";
import { SessionRegistry, type RegisteredSession } from "./registry.js";
import { SessionManager } from "./sessions.js";
import { FakeBroadcaster, FakeHarnessRegistry, StubDriver, StubSession, tmpDir } from "./testutil.js";

function entry(over: Partial<RegisteredSession> = {}): RegisteredSession {
  return {
    id: "s_1",
    harnessSessionId: "thread_1",
    harness: "codex",
    cwd: "/tmp/work",
    createdAt: 1000,
    lastEventAt: 1000,
    lastState: "idle",
    closed: false,
    interrupted: false,
    ...over,
  };
}

async function waitFor(cond: () => boolean, timeout = 2000): Promise<void> {
  const start = Date.now();
  while (!cond()) {
    if (Date.now() - start > timeout) throw new Error("waitFor 超时");
    await new Promise((r) => setTimeout(r, 5));
  }
}

function makeManager(opts: { driver?: StubDriver; registry?: SessionRegistry } = {}) {
  const dataDir = tmpDir("amux-sm-");
  const registry = opts.registry ?? new SessionRegistry(join(dataDir, "sessions.json"));
  const history = new HistoryStore(join(dataDir, "history"));
  const broadcast = new FakeBroadcaster();
  const sessions = new Map<string, StubSession>();
  const driver = opts.driver ?? new StubDriver({ sessions });
  const harnesses = new FakeHarnessRegistry(new Map([["codex", driver]]));
  const manager = new SessionManager({ registry, history, broadcast, harnesses });
  return { manager, registry, history, broadcast, driver, sessions, dataDir };
}

const text = (t: string) => [{ type: "text" as const, text: t }];

/** 单会话测试场景下取唯一的 StubSession（桩 id 由 stub 自己生成，与 server wire id 不同）。 */
function stubOf(sessions: Map<string, StubSession>): StubSession {
  const s = sessions.values().next().value;
  if (!s) throw new Error("没有 stub session");
  return s;
}

describe("SessionManager 生命周期", () => {
  it("create：建会话、入注册表、广播 session_created", async () => {
    const { manager, broadcast, sessions } = makeManager();
    const meta = await manager.create({ harness: "codex", cwd: "/tmp/work" });
    expect(meta.id).toMatch(/^s_/);
    expect(meta.harness).toBe("codex");
    expect(meta.state).toBe("idle");
    expect(stubOf(sessions).cwd).toBe("/tmp/work");
    expect(broadcast.last()?.method).toBe("session_created");
    expect(manager.list().map((s) => s.id)).toEqual([meta.id]);
  });

  it("create 时 harness 不可用 → HarnessUnavailableError，不产生注册表条目", async () => {
    const { manager } = makeManager({ driver: new StubDriver({ failCreate: true }) });
    await expect(manager.create({ harness: "codex", cwd: "/tmp" })).rejects.toThrow(HarnessUnavailableError);
    expect(manager.list()).toEqual([]);
  });

  it("resume：恢复会话并清除 interrupted/closed 标记", async () => {
    const { manager, registry, sessions } = makeManager();
    registry.upsert(entry({ id: "s_old", cwd: "/tmp", lastState: "thinking", interrupted: true }));
    registry.save();
    const meta = await manager.resume("s_old");
    expect(meta.interrupted).toBe(false);
    expect(meta.closed).toBe(false);
    expect(meta.state).toBe("idle");
    expect(sessions.has("thread_1")).toBe(true); // 恢复按 harnessSessionId（resume 键）
  });

  it("resume 不存在的会话 → SessionNotFoundError", async () => {
    const { manager } = makeManager();
    await expect(manager.resume("nope")).rejects.toThrow(SessionNotFoundError);
  });

  it("cancel：调用驱动 cancel", async () => {
    const { manager, sessions } = makeManager();
    const meta = await manager.create({ harness: "codex", cwd: "/tmp" });
    await manager.cancel(meta.id);
    expect(stubOf(sessions).cancelCalls).toBe(1);
  });

  it("close：驱动关闭、标 closed、广播 session_closed；close 后 prompt 报 SessionClosedError", async () => {
    const { manager, broadcast, sessions } = makeManager();
    const meta = await manager.create({ harness: "codex", cwd: "/tmp" });
    await manager.close(meta.id);
    expect(stubOf(sessions).closed).toBe(true);
    expect(broadcast.last()?.method).toBe("session_closed");
    expect(manager.list()[0].closed).toBe(true);
    await expect(manager.prompt(meta.id, text("x"))).rejects.toThrow(SessionClosedError);
  });

  it("delete：移除注册表与历史、广播 session_deleted", async () => {
    const { manager, broadcast, history, sessions } = makeManager();
    const meta = await manager.create({ harness: "codex", cwd: "/tmp" });
    stubOf(sessions).pushEvent({ kind: "agent_message", messageId: "m", content: text("hi") });
    await waitFor(() => history.load(meta.id).length === 1);
    await manager.delete(meta.id);
    expect(manager.list()).toEqual([]);
    expect(history.load(meta.id)).toEqual([]);
    expect(broadcast.last()?.method).toBe("session_deleted");
    await expect(manager.prompt(meta.id, text("x"))).rejects.toThrow(SessionNotFoundError);
  });

  it("prompt 在 interrupted 会话上报 SessionClosedError", async () => {
    const { manager, registry } = makeManager();
    registry.upsert(entry({ id: "s_int", lastState: "thinking", interrupted: true }));
    registry.save();
    await expect(manager.prompt("s_int", text("x"))).rejects.toThrow(SessionClosedError);
  });

  it("多客户端并发 prompt 按到达顺序串行化", async () => {
    const { manager, sessions } = makeManager();
    const meta = await manager.create({ harness: "codex", cwd: "/tmp" });
    const stub = stubOf(sessions);
    let release!: () => void;
    stub.promptGate = new Promise((r) => (release = r));
    const p1 = manager.prompt(meta.id, text("一"));
    await new Promise((r) => setTimeout(r, 10));
    const p2 = manager.prompt(meta.id, text("二"));
    await new Promise((r) => setTimeout(r, 10));
    expect(stub.promptCalls.length).toBe(1); // 第二个被串行化，尚未开始
    release();
    await Promise.all([p1, p2]);
    expect(stub.promptCalls.map((c) => (c[0] as { text: string }).text)).toEqual(["一", "二"]);
  });

  it("prompt 持久化用户输入（同一 jsonl，混合按序）并广播 user_message 通知（不带序号）", async () => {
    const { manager, history, broadcast, sessions } = makeManager();
    const meta = await manager.create({ harness: "codex", cwd: "/tmp" });
    await manager.prompt(meta.id, text("你好"));
    const stub = stubOf(sessions);
    stub.pushEvent({ kind: "agent_message", messageId: "m1", content: text("回复") }, 2000);

    await waitFor(
      () =>
        broadcast.notifications.filter((n) => n.method === "user_message").length === 1 &&
        broadcast.notifications.filter((n) => n.method === "event").length === 1,
    );

    // 用户输入以 user_message 通知广播（非 event），不带 seq
    const um = broadcast.notifications.find((n) => n.method === "user_message")!.params as { content: unknown; timestamp: number };
    expect(um.content).toEqual(text("你好"));
    expect("seq" in um).toBe(false);
    const evParams = broadcast.notifications.filter((n) => n.method === "event").map((n) => n.params as { event: { kind: string } });
    expect(evParams[0]).toMatchObject({ event: { kind: "agent_message" } });
    expect("seq" in evParams[0]).toBe(false);

    // 会话历史：单个 jsonl，用户输入与事件混合按序（非拆分文件、不带序号）
    const hist = history.load(meta.id);
    expect(hist).toHaveLength(2);
    expect(hist[0]).toMatchObject({ content: text("你好") });
    expect(hist[1]).toMatchObject({ event: { kind: "agent_message" } });
    expect("seq" in hist[0]).toBe(false);
    expect("seq" in hist[1]).toBe(false);
  });
});

describe("SessionManager 事件管道", () => {
  it("事件按序广播（不带序号）、历史只含非 chunk、状态更新", async () => {
    const { manager, history, broadcast, sessions } = makeManager();
    const meta = await manager.create({ harness: "codex", cwd: "/tmp" });
    const stub = stubOf(sessions);
    stub.pushEvent({ kind: "agent_thought", messageId: "m1" }, 1000);
    stub.pushEvent({ kind: "agent_thought_chunk", messageId: "m1", content: { type: "text", text: "思考" } }, 1001);
    stub.pushEvent({ kind: "agent_message", messageId: "m2", content: text("hi") }, 1002);
    stub.pushEvent({ kind: "state_changed", state: "thinking" }, 1003);
    stub.pushEvent({ kind: "state_changed", state: "idle", reason: "end_turn" }, 1004);

    await waitFor(() => broadcast.notifications.filter((n) => n.method === "event").length === 5);

    const events = broadcast.notifications.filter((n) => n.method === "event");
    for (const n of events) {
      const p = n.params as { sessionId: string };
      expect(p.sessionId).toBe(meta.id);
      expect("seq" in p).toBe(false);
    }
    // 广播顺序 = 投递顺序
    expect((events[0].params as { event: { kind: string } }).event.kind).toBe("agent_thought");

    // 历史只保存对话内容（思考/工具调用/输出）：状态、用量、错误、chunk 不落盘
    const hist = history.load(meta.id);
    expect(hist.map((h) => (h as { event: { kind: string } }).event.kind)).toEqual(["agent_thought", "agent_message"]);

    // 状态经 state_changed 更新并持久化
    const m = manager.list().find((s) => s.id === meta.id)!;
    expect(m.state).toBe("idle");
  });

  it("anyBusyInCwd：会话忙时 true，空闲/其他目录 false", async () => {
    const { manager, sessions } = makeManager();
    const meta = await manager.create({ harness: "codex", cwd: "/tmp/busy" });
    stubOf(sessions).pushEvent({ kind: "state_changed", state: "acting" });
    await waitFor(() => manager.anyBusyInCwd("/tmp/busy"));
    expect(manager.anyBusyInCwd("/tmp/other")).toBe(false);
  });

  it("historyFor：会话不存在报 SessionNotFoundError", async () => {
    const { manager } = makeManager();
    expect(() => manager.historyFor("nope")).toThrow(SessionNotFoundError);
  });

  it("currentOrder：内部序号随事件递增，供 get_history 标记补齐位置", async () => {
    const { manager, sessions } = makeManager();
    const meta = await manager.create({ harness: "codex", cwd: "/tmp" });
    expect(manager.currentOrder(meta.id)).toBe(0);
    const stub = stubOf(sessions);
    stub.pushEvent({ kind: "agent_message", messageId: "m", content: text("hi") });
    await waitFor(() => manager.currentOrder(meta.id) === 1);
    stub.pushEvent({ kind: "state_changed", state: "idle", reason: "end_turn" });
    await waitFor(() => manager.currentOrder(meta.id) === 2);
  });
});

describe("SessionManager 重启恢复", () => {
  it("忙会话标 interrupted；空闲会话自动恢复；恢复失败标 interrupted；closed 保持", async () => {
    const dataDir = tmpDir("amux-restore-");
    const registry = new SessionRegistry(join(dataDir, "sessions.json"));
    registry.upsert(entry({ id: "busy", harnessSessionId: "h_busy", cwd: "/tmp/a", lastState: "thinking" }));
    registry.upsert(entry({ id: "idle_ok", harnessSessionId: "h_ok", cwd: "/tmp/b", lastState: "idle" }));
    registry.upsert(entry({ id: "idle_fail", harnessSessionId: "h_fail", cwd: "/tmp/c", lastState: "idle" }));
    registry.upsert(entry({ id: "closed", harnessSessionId: "h_closed", cwd: "/tmp/d", lastState: "thinking", closed: true }));
    const driver = new StubDriver({ sessions: new Map() });
    const origResume = driver.resumeSession.bind(driver);
    driver.resumeSession = async (id: string, cwd?: string) => {
      if (id === "h_fail") throw new SessionNotFoundError("不存在");
      return origResume(id, cwd);
    };
    const history = new HistoryStore(join(dataDir, "history"));
    const broadcast = new FakeBroadcaster();
    const harnesses = new FakeHarnessRegistry(new Map([["codex", driver]]));
    const manager = new SessionManager({ registry, history, broadcast, harnesses });

    await manager.restore();

    const byId = new Map(manager.list().map((s) => [s.id, s]));
    expect(byId.get("busy")?.interrupted).toBe(true); // 忙 → interrupted，未自动恢复
    expect(byId.get("busy")?.state).toBe("thinking");
    expect(byId.get("idle_ok")?.interrupted).toBe(false); // 空闲 → 自动恢复
    expect(byId.get("idle_fail")?.interrupted).toBe(true); // 恢复失败 → interrupted
    expect(byId.get("closed")?.closed).toBe(true); // closed 保持
    expect(byId.get("closed")?.interrupted).toBe(false);

    // interrupted 会话可被客户端恢复，事件继续
    const meta = await manager.resume("busy");
    expect(meta.interrupted).toBe(false);
    expect(meta.state).toBe("idle");
  });
});
