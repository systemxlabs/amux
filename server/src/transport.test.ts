import { join } from "node:path";
import { WebSocket } from "ws";
import { describe, expect, it } from "vitest";
import { Broadcaster } from "./broadcast.js";
import { EventBuffer } from "./eventbuffer.js";
import { GitRunner } from "./git.js";
import { HarnessRegistry } from "./harness.js";
import { HistoryStore } from "./history.js";
import { encodeMessage, errorResponse, parseJsonRpc } from "./jsonrpc.js";
import { SessionRegistry } from "./registry.js";
import { RpcServer } from "./rpc.js";
import { buildMethodHandlers } from "./rpchandler.js";
import { SessionManager } from "./sessions.js";
import { StubDriver, StubSession, tmpDir } from "./testutil.js";
import { Transport } from "./transport.js";

async function waitFor(cond: () => boolean, timeout = 2000): Promise<void> {
  const start = Date.now();
  while (!cond()) {
    if (Date.now() - start > timeout) throw new Error("waitFor 超时");
    await new Promise((r) => setTimeout(r, 5));
  }
}

interface TestServer {
  url: string;
  port: number;
  manager: SessionManager;
  sessions: Map<string, StubSession>;
  close: () => Promise<void>;
}

async function startTestServer(): Promise<TestServer> {
  const dataDir = tmpDir("amux-transport-");
  const registry = new SessionRegistry(join(dataDir, "sessions.json"));
  registry.load();
  const history = new HistoryStore(join(dataDir, "history"));
  const buffer = new EventBuffer(100);
  const broadcaster = new Broadcaster();
  const sessions = new Map<string, StubSession>();
  const driver = new StubDriver({ sessions });
  const harnesses = new HarnessRegistry([
    { name: "codex", createDriver: () => driver, probe: () => true },
    { name: "claude", createDriver: () => driver, probe: () => true },
    { name: "kimi", createDriver: () => new StubDriver({ failCreate: true }), probe: () => true },
  ]);
  const manager = new SessionManager({ registry, history, buffer, broadcast: broadcaster, harnesses });
  const git = new GitRunner();
  const rpc = new RpcServer();
  for (const [m, h] of Object.entries(buildMethodHandlers({ manager, git, harnesses, serverVersion: "test" }))) {
    rpc.register(m, h);
  }
  const token = "secret-token";
  const transport = new Transport({
    host: "127.0.0.1",
    port: 0,
    token,
    onConnection: (s) => broadcaster.add(s),
    onClose: (s) => broadcaster.remove(s),
    onMessage: (s, text) => {
      const frame = parseJsonRpc(text);
      if (frame.kind === "error") {
        s.send(encodeMessage(errorResponse(frame.error)));
        return;
      }
      if (frame.kind === "request" || frame.kind === "notification") {
        void rpc
          .handle(frame.kind === "request" ? frame.request : frame.notification)
          .then((res) => {
            if (res && frame.kind === "request") s.send(encodeMessage(res));
          })
          .catch(() => {
            // 通知类错误不回响应
          });
      }
    },
  });
  await transport.start();
  const addr = transport.address();
  return {
    url: `ws://127.0.0.1:${addr.port}?token=${token}`,
    port: addr.port,
    manager,
    sessions,
    close: () => transport.close(),
  };
}

function connect(url: string): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url);
    ws.on("open", () => resolve(ws));
    ws.on("error", reject);
  });
}

function request(ws: WebSocket, id: number, method: string, params?: unknown): Promise<Record<string, unknown>> {
  return new Promise((resolve) => {
    const onMsg = (data: Buffer) => {
      const msg = JSON.parse(data.toString()) as Record<string, unknown>;
      if (msg.id === id) {
        ws.off("message", onMsg);
        resolve(msg);
      }
    };
    ws.on("message", onMsg);
    ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, ...(params !== undefined ? { params } : {}) }));
  });
}

describe("Transport + RpcServer（真实 WebSocket，原始 JSON-RPC 帧）", () => {
  it("无 token / 错误 token 的连接被拒绝（4401）", async () => {
    const srv = await startTestServer();
    try {
      const bad = (port: number, url: string) =>
        new Promise<number>((resolve) => {
          const ws = new WebSocket(url);
          ws.on("close", (c) => resolve(c));
          ws.on("error", () => {
            /* 关闭事件兜底 */
          });
        });
      expect(await bad(srv.port, `ws://127.0.0.1:${srv.port}/?token=wrong`)).toBe(4401);
      expect(await bad(srv.port, `ws://127.0.0.1:${srv.port}/`)).toBe(4401);
    } finally {
      await srv.close();
    }
  });

  it("get_info：正确 token 返回 harness 列表（探测结果）", async () => {
    const srv = await startTestServer();
    try {
      const ws = await connect(srv.url);
      const res = await request(ws, 1, "get_info", {});
      expect(res.id).toBe(1);
      const info = (res.result as { info: { serverVersion: string; harnesses: Array<{ name: string; available: boolean }> } }).info;
      expect(info.serverVersion).toBe("test");
      expect(info.harnesses.find((h) => h.name === "codex")?.available).toBe(true);
      expect(info.harnesses.find((h) => h.name === "kimi")?.available).toBe(true);
      ws.close();
    } finally {
      await srv.close();
    }
  });

  it("未知方法 → -32601；畸形 JSON → -32700（id:null）", async () => {
    const srv = await startTestServer();
    try {
      const ws = await connect(srv.url);
      const res = await request(ws, 2, "no_such_method", {});
      expect(res.error).toMatchObject({ code: -32601 });
      const parseRes = await new Promise<Record<string, unknown>>((resolve) => {
        ws.on("message", (d) => resolve(JSON.parse(d.toString())));
        ws.send("{bad json");
      });
      expect(parseRes.error).toMatchObject({ code: -32700 });
      expect(parseRes.id).toBeNull();
      ws.close();
    } finally {
      await srv.close();
    }
  });

  it("非法参数 → -32602；输入内容非法 → -32005", async () => {
    const srv = await startTestServer();
    try {
      const ws = await connect(srv.url);
      const bad1 = await request(ws, 3, "create_session", { harness: "codex" }); // 缺 cwd
      expect(bad1.error).toMatchObject({ code: -32602 });
      const bad2 = await request(ws, 4, "create_session", { harness: "nope", cwd: "/tmp" });
      expect(bad2.error).toMatchObject({ code: -32602 });
      const ok = await request(ws, 5, "create_session", { harness: "codex", cwd: "/tmp" });
      const sessionId = (ok.result as { session: { id: string } }).session.id;
      const bad3 = await request(ws, 6, "prompt", { sessionId, input: [{ type: "wat" }] });
      expect(bad3.error).toMatchObject({ code: -32005 });
      ws.close();
    } finally {
      await srv.close();
    }
  });

  it("create_session：harness 不可用 → -32002", async () => {
    const srv = await startTestServer();
    try {
      const ws = await connect(srv.url);
      const res = await request(ws, 1, "create_session", { harness: "kimi", cwd: "/tmp" });
      expect(res.error).toMatchObject({ code: -32002 });
      ws.close();
    } finally {
      await srv.close();
    }
  });

  it("prompt（idle 启动 / 忙时 steer）与 cancel 经真实协议生效", async () => {
    const srv = await startTestServer();
    try {
      const ws = await connect(srv.url);
      const res = await request(ws, 1, "create_session", { harness: "codex", cwd: "/tmp" });
      const sessionId = (res.result as { session: { id: string } }).session.id;
      const stub = [...srv.sessions.values()][0]!;

      await request(ws, 2, "prompt", { sessionId, input: [{ type: "text", text: "启动" }] });
      expect(stub.promptCalls[0][0]).toEqual({ type: "text", text: "启动" });

      stub.pushEvent({ kind: "state_changed", state: "thinking" });
      await waitFor(() => srv.manager.list().find((s) => s.id === sessionId)!.state === "thinking");

      // 忙时 prompt = steer，第二个 prompt 照常送达
      await request(ws, 3, "prompt", { sessionId, input: [{ type: "text", text: "steer" }] });
      expect(stub.promptCalls[1][0]).toEqual({ type: "text", text: "steer" });

      await request(ws, 4, "cancel", { sessionId });
      expect(stub.cancelCalls).toBe(1);
      ws.close();
    } finally {
      await srv.close();
    }
  });

  it("广播：两个客户端收到同一份按序事件流，互不踢出", async () => {
    const srv = await startTestServer();
    try {
      const wsA = await connect(srv.url);
      const wsB = await connect(srv.url);
      const eventsA: Array<{ seq: number; event: { kind: string } }> = [];
      const eventsB: Array<{ seq: number; event: { kind: string } }> = [];
      wsA.on("message", (d) => {
        const m = JSON.parse(d.toString());
        if (m.method === "event") eventsA.push(m.params);
      });
      wsB.on("message", (d) => {
        const m = JSON.parse(d.toString());
        if (m.method === "event") eventsB.push(m.params);
      });

      const res = await request(wsA, 10, "create_session", { harness: "codex", cwd: "/tmp" });
      const sessionId = (res.result as { session: { id: string } }).session.id;
      const stub = [...srv.sessions.values()][0]!;

      stub.pushEvent({ kind: "agent_message", messageId: "m", content: [{ type: "text", text: "hi" }] });
      await waitFor(() => eventsA.length === 1 && eventsB.length === 1);
      expect(eventsA[0]).toEqual(eventsB[0]);
      expect(eventsA[0].seq).toBe(1);
      expect(eventsA[0].event.kind).toBe("agent_message");

      stub.pushEvent({ kind: "state_changed", state: "idle", reason: "end_turn" });
      await waitFor(() => eventsA.length === 2 && eventsB.length === 2);
      expect(eventsA[1]).toEqual(eventsB[1]);
      expect(eventsA[1].event.kind).toBe("state_changed");

      wsA.close();
      wsB.close();
    } finally {
      await srv.close();
    }
  });

  it("重连补齐：历史 + 缓冲缺口恰好一次、不重复", async () => {
    const srv = await startTestServer();
    try {
      const ws1 = await connect(srv.url);
      const res = await request(ws1, 1, "create_session", { harness: "codex", cwd: "/tmp" });
      const sessionId = (res.result as { session: { id: string } }).session.id;
      const stub = [...srv.sessions.values()][0]!;

      // 第一段：完整对话事件（非 chunk，落历史）
      stub.pushEvent({ kind: "agent_thought", messageId: "t1" });
      stub.pushEvent({ kind: "agent_message", messageId: "m1", content: [{ type: "text", text: "done" }] });
      stub.pushEvent({ kind: "state_changed", state: "idle", reason: "end_turn" });
      await waitFor(() => srv.manager.historyFor(sessionId).length === 3);
      ws1.close();

      // 第二段开始：chunk 先到（流式片段，不入历史，只进缓冲）
      stub.pushEvent({ kind: "agent_message_chunk", messageId: "m2", content: { type: "text", text: "半" } });
      await waitFor(() => srv.manager.bufferedFor(sessionId).length === 4);

      // 流式中途重连：历史（1-3）+ 缓冲补齐缺口（4）
      const ws2 = await connect(srv.url);
      const hist = await request(ws2, 2, "get_history", { sessionId });
      const histEvents = (hist.result as { events: Array<{ seq: number }> }).events;
      expect(histEvents.map((e) => e.seq)).toEqual([1, 2, 3]);
      const lastSeq = histEvents[histEvents.length - 1].seq;
      const buf = await request(ws2, 3, "get_buffered_events", { sessionId, afterSeq: lastSeq });
      const bufEvents = (buf.result as { events: Array<{ seq: number }> }).events;
      expect(bufEvents.map((e) => e.seq)).toEqual([4]);

      // 续上实时流：5、6 经广播到达，合并后恰好一次、不重复
      const live: Array<{ seq: number }> = [];
      ws2.on("message", (d) => {
        const m = JSON.parse(d.toString());
        if (m.method === "event" && m.params.sessionId === sessionId) live.push(m.params);
      });
      stub.pushEvent({ kind: "agent_message", messageId: "m2", content: [{ type: "text", text: "完整" }] });
      stub.pushEvent({ kind: "state_changed", state: "idle", reason: "end_turn" });
      await waitFor(() => live.length === 2);
      const merged = [...histEvents, ...bufEvents, ...live.map((e) => ({ seq: e.seq }))];
      expect(merged.map((e) => e.seq)).toEqual([1, 2, 3, 4, 5, 6]); // 恰好一次、顺序完整
      ws2.close();
    } finally {
      await srv.close();
    }
  });

  it("git_revert：会话忙时被拒绝（-32003，undo 需等工作区间结束）", async () => {
    const srv = await startTestServer();
    try {
      const ws = await connect(srv.url);
      const res = await request(ws, 1, "create_session", { harness: "codex", cwd: "/tmp/gitbusy" });
      const sessionId = (res.result as { session: { id: string } }).session.id;
      const stub = [...srv.sessions.values()][0]!;
      stub.pushEvent({ kind: "state_changed", state: "acting" });
      await waitFor(() => srv.manager.anyBusyInCwd("/tmp/gitbusy"));

      const rv = await request(ws, 2, "git_revert", { cwd: "/tmp/gitbusy" });
      expect(rv.error).toMatchObject({ code: -32003 });
      ws.close();
    } finally {
      await srv.close();
    }
  });
});
