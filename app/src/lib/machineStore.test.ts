import { describe, expect, it } from "vitest";
import type { SessionMeta } from "shared";
import { MachineStore } from "./machineStore.js";
import { SessionFeed } from "./sessionFeed.js";

const meta = (id: string, over: Partial<SessionMeta> = {}): SessionMeta => ({
  id,
  harness: "codex",
  cwd: "/tmp",
  createdAt: 1,
  lastEventAt: 1,
  state: "idle",
  interrupted: false,
  closed: false,
  ...over,
});

/** 通过 private 方法驱动真实通知处理（不打开真实连接）。 */
function notify(store: MachineStore, method: string, params: unknown): void {
  (store as unknown as { onNotification(m: string, p: unknown): void }).onNotification(method, params);
}

describe("MachineStore 会话列表通知", () => {
  it("session_deleted：从列表与事件流移除（不再 upsert 回列表）", () => {
    const s = new MachineStore({ id: "m1", name: "测试机", url: "ws://127.0.0.1:9", token: "t" });
    s.state.sessions.push(meta("s1"), meta("s2"));
    s.state.feeds.set("s1", new SessionFeed("s1"));
    notify(s, "session_deleted", { session: meta("s1") });
    expect(s.state.sessions.map((x) => x.id)).toEqual(["s2"]);
    expect(s.state.feeds.has("s1")).toBe(false);
  });

  it("session_closed：保留在列表并标记 closed（可恢复）", () => {
    const s = new MachineStore({ id: "m1", name: "测试机", url: "ws://127.0.0.1:9", token: "t" });
    s.state.sessions.push(meta("s1"));
    notify(s, "session_closed", { session: meta("s1", { closed: true }) });
    expect(s.state.sessions.map((x) => x.id)).toEqual(["s1"]);
    expect(s.state.sessions[0].closed).toBe(true);
  });

  it("session_created：加入列表", () => {
    const s = new MachineStore({ id: "m1", name: "测试机", url: "ws://127.0.0.1:9", token: "t" });
    notify(s, "session_created", { session: meta("s9") });
    expect(s.state.sessions.map((x) => x.id)).toEqual(["s9"]);
  });

  it("实时 state_changed 同步到会话 meta.state（状态不落历史，靠 meta 保持最新）", () => {
    const s = new MachineStore({ id: "m1", name: "测试机", url: "ws://127.0.0.1:9", token: "t" });
    s.state.sessions.push(meta("s1"));
    s.state.feeds.set("s1", new SessionFeed("s1"));
    notify(s, "event", { sessionId: "s1", event: { kind: "state_changed", state: "thinking" }, timestamp: 1 });
    expect(s.state.sessions[0].state).toBe("thinking");
    notify(s, "event", { sessionId: "s1", event: { kind: "state_changed", state: "idle", reason: "end_turn" }, timestamp: 2 });
    expect(s.state.sessions[0].state).toBe("idle");
  });

  it("实时事件按到达顺序追加到 feed（不带 seq），并进入通知队列", () => {
    const s = new MachineStore({ id: "m1", name: "测试机", url: "ws://127.0.0.1:9", token: "t" });
    s.state.sessions.push(meta("s1"));
    s.state.feeds.set("s1", new SessionFeed("s1"));
    notify(s, "event", { sessionId: "s1", event: { kind: "agent_message", messageId: "m1" }, timestamp: 1 });
    notify(s, "user_message", { sessionId: "s1", content: [{ type: "text", text: "hi" }], timestamp: 2 });
    notify(s, "event", { sessionId: "s1", event: { kind: "agent_message", messageId: "m2" }, timestamp: 3 });
    const feed = s.state.feeds.get("s1")!;
    expect(feed.events).toHaveLength(3);
    expect("seq" in feed.events[0]).toBe(false);
    expect(feed.drainNotify()).toHaveLength(3);
  });
});
