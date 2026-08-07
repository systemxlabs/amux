import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { WebSocket } from "ws";
import { describe, expect, it } from "vitest";
import { Broadcaster } from "./broadcast.js";
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
  const broadcaster = new Broadcaster();
  const sessions = new Map<string, StubSession>();
  const driver = new StubDriver({ sessions });
  const harnesses = new HarnessRegistry([
    { name: "codex", createDriver: () => driver, probe: () => true },
    { name: "claude", createDriver: () => driver, probe: () => true },
    { name: "kimi", createDriver: () => new StubDriver({ failCreate: true }), probe: () => true },
  ]);
  const manager = new SessionManager({ registry, history, broadcast: broadcaster, harnesses });
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
        const catchup = broadcaster.catchupFor(s);
        const ctx =
          catchup === undefined
            ? undefined
            : {
                catchup,
                send: (method: string, params: unknown) => broadcaster.sendTo(s, method, params),
              };
        void rpc
          .handle(frame.kind === "request" ? frame.request : frame.notification, ctx)
          .then(({ response, afterSend }) => {
            if (response && frame.kind === "request") s.send(encodeMessage(response));
            afterSend?.();
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

function git(cwd: string, ...args: string[]): string {
  return execFileSync("git", ["-C", cwd, ...args], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
}

/** 建一个真实 git 仓库（有初始提交），供 git_status 端到端用。 */
function initRepo(): string {
  const dir = tmpDir("amux-tr-git-");
  git(dir, "init", "-b", "main", "-q");
  git(dir, "config", "user.email", "t@t");
  git(dir, "config", "user.name", "t");
  writeFileSync(join(dir, "a.txt"), "line1\nline2\n");
  git(dir, "add", ".");
  git(dir, "commit", "-m", "init", "-q");
  return dir;
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

  it("广播：两个客户端收到同一份按序事件流（不带序号），互不踢出", async () => {
    const srv = await startTestServer();
    try {
      const wsA = await connect(srv.url);
      const wsB = await connect(srv.url);
      const eventsA: Array<{ event: { kind: string }; seq?: number }> = [];
      const eventsB: Array<{ event: { kind: string }; seq?: number }> = [];
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

      // 两个客户端都先 get_history（标记补齐位置），避免事件被暂存
      await request(wsA, 11, "get_history", { sessionId });
      await request(wsB, 12, "get_history", { sessionId });

      stub.pushEvent({ kind: "agent_message", messageId: "m", content: [{ type: "text", text: "hi" }] });
      await waitFor(() => eventsA.length === 1 && eventsB.length === 1);
      expect(eventsA[0]).toEqual(eventsB[0]);
      expect(eventsA[0].event.kind).toBe("agent_message");
      expect(eventsA[0].seq).toBeUndefined();

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

  it("重连补齐（server 按连接对齐）：历史 + 补齐后实时恰好一次、顺序完整", async () => {
    const srv = await startTestServer();
    try {
      const ws1 = await connect(srv.url);
      const res = await request(ws1, 1, "create_session", { harness: "codex", cwd: "/tmp" });
      const sessionId = (res.result as { session: { id: string } }).session.id;
      const stub = [...srv.sessions.values()][0]!;

      // 第一段：对话事件（thought、message 落历史；state_changed 不落）
      stub.pushEvent({ kind: "agent_thought", messageId: "t1" });
      stub.pushEvent({ kind: "agent_message", messageId: "m1", content: [{ type: "text", text: "done" }] });
      stub.pushEvent({ kind: "state_changed", state: "idle", reason: "end_turn" });
      await waitFor(() => srv.manager.historyFor(sessionId).length === 2);
      ws1.close();

      // 第二段：ws2 连接（尚未 get_history，该会话的实时项被 server 按连接暂存）
      const ws2 = await connect(srv.url);
      const live: Array<{ event: { kind: string; messageId?: string } }> = [];
      ws2.on("message", (d) => {
        const m = JSON.parse(d.toString());
        if (m.method === "event" && m.params.sessionId === sessionId) live.push(m.params);
      });

      // 连接建立后到达：chunk（不落盘，流式）+ 完整 message（落盘）——都应被暂存，不直接广播
      stub.pushEvent({ kind: "agent_message_chunk", messageId: "m2", content: { type: "text", text: "半" } });
      stub.pushEvent({ kind: "agent_message", messageId: "m2", content: [{ type: "text", text: "完整" }] });
      await waitFor(() => srv.manager.currentOrder(sessionId) === 5);
      // 暂存期间客户端收不到任何该会话通知
      expect(live.length).toBe(0);

      // get_history：历史快照与补齐位置原子对齐（含 m2 完整消息，chunk 由完整消息收敛）
      const hist = await request(ws2, 2, "get_history", { sessionId });
      const histItems = (hist.result as { items: Array<{ event: { kind: string; messageId: string }; seq?: number }> }).items;
      expect(histItems.map((e) => e.event.kind)).toEqual(["agent_thought", "agent_message", "agent_message"]);
      expect(histItems.map((e) => e.event.messageId)).toEqual(["t1", "m1", "m2"]);
      for (const it of histItems) expect("seq" in it).toBe(false); // 历史记录不带序号

      // 补齐后实时事件直接广播（不重复历史）
      stub.pushEvent({ kind: "agent_message", messageId: "m3", content: [{ type: "text", text: "继续" }] });
      await waitFor(() => live.length === 1);
      expect(live[0].event.messageId).toBe("m3");

      // 客户端视角合并：历史 + 实时 = 恰好一次、按真实顺序（chunk/状态不参与对话内容）
      const merged = [...histItems.map((e) => e.event.messageId), ...live.map((e) => e.event.messageId)];
      expect(merged).toEqual(["t1", "m1", "m2", "m3"]);

      // get_buffered_events 已从协议移除
      const gone = await request(ws2, 3, "get_buffered_events", { sessionId });
      expect(gone.error).toMatchObject({ code: -32601 });

      ws2.close();
    } finally {
      await srv.close();
    }
  });

  it("git_status：真实 git 仓库返回正确形状（branch/changes）；非 git 仓库返回 notRepo 标记且 server 不崩溃", async () => {
    const srv = await startTestServer();
    try {
      const ws = await connect(srv.url);
      // 真实 git 仓库：result 必须含 branch 与 changes 数组（回归：曾因缺 await 把 Promise 序列化为 {}）
      const repo = initRepo();
      const ok = await request(ws, 1, "git_status", { cwd: repo });
      expect(ok.error).toBeUndefined();
      const result = ok.result as { branch: string; changes: Array<{ path: string }>; notRepo?: boolean };
      expect(result.branch).toBe("main");
      expect(Array.isArray(result.changes)).toBe(true);
      expect(result.notRepo).toBeUndefined();

      // 非 git 仓库（存在但未初始化）：成功响应 + notRepo 标记（而非错误/空对象），server 进程不受影响
      const plainDir = tmpDir("amux-notrepo-");
      const notRepo = await request(ws, 2, "git_status", { cwd: plainDir });
      expect(notRepo.error).toBeUndefined();
      expect((notRepo.result as { notRepo: boolean }).notRepo).toBe(true);

      // server 仍存活：后续请求正常响应
      const info = await request(ws, 3, "get_info", {});
      expect(info.error).toBeUndefined();
      ws.close();
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
