/**
 * codex driver：spawn `codex app-server`（stdio / JSON-RPC 2.0），
 * 管理 thread（= Session）与 turn（= 工作区间）。
 *
 * yolo 模式：approvalPolicy "never" + sandboxPolicy dangerFullAccess。
 * resume：thread/resume（~/.codex/sessions 持久化）。
 * cancel：turn/interrupt。
 */
import {
  HarnessUnavailableError,
  InvalidInputError,
  SessionClosedError,
  SessionNotFoundError,
  type ContentBlock,
  type Driver,
  type Event,
  type Input,
  type Session,
  type SessionEvent,
  type SessionOptions,
} from "ahal";
import { JsonRpcClient, type JsonRpcNotification } from "./jsonrpc.js";
import { CodexNormalizer, type CodexItem, type CodexWireEvent } from "./normalize.js";

const YOLO_PARAMS = {
  approvalPolicy: "never",
  sandboxPolicy: { type: "dangerFullAccess" },
} as const;

/** 把 AHAL 内容块转为 codex 输入文本项 */
function toCodexInput(input: Input): { type: "text"; text: string }[] {
  const out: { type: "text"; text: string }[] = [];
  for (const block of input) {
    if (block.type === "text") {
      out.push({ type: "text", text: block.text });
    } else if (block.type === "resource" && "text" in block) {
      out.push({ type: "text", text: block.text });
    } else if (block.type === "resource_link") {
      out.push({ type: "text", text: block.uri });
    } else {
      throw new InvalidInputError("二进制资源（blob）暂不支持");
    }
  }
  return out;
}

/** 极简 hot stream 广播器：多订阅、订阅时刻起接收后续事件 */
class Broadcaster<T> {
  private subs = new Set<Subscriber<T>>();
  private closed = false;

  emit(item: T): void {
    for (const s of [...this.subs]) s.push(item);
  }

  subscribe(): AsyncIterable<T> {
    const sub = new Subscriber<T>(() => {
      this.subs.delete(sub);
    });
    this.subs.add(sub);
    return sub;
  }

  close(): void {
    this.closed = true;
    for (const s of [...this.subs]) s.end();
    this.subs.clear();
  }
}

class Subscriber<T> {
  private queue: T[] = [];
  private waiting: { resolve: () => void } | null = null;
  private done = false;
  constructor(private readonly onEnd: () => void) {}

  push(item: T): void {
    if (this.done) return;
    this.queue.push(item);
    this.waiting?.resolve();
    this.waiting = null;
  }

  end(): void {
    if (this.done) return;
    this.done = true;
    this.waiting?.resolve();
    this.waiting = null;
  }

  async *[Symbol.asyncIterator](): AsyncIterator<T> {
    while (!this.done || this.queue.length > 0) {
      if (this.queue.length > 0) {
        yield this.queue.shift() as T;
        continue;
      }
      if (this.done) break;
      await new Promise<void>((resolve) => {
        this.waiting = { resolve };
      });
    }
    this.onEnd();
  }
}

interface SessionRuntime {
  threadId: string;
  cwd: string;
  turnId: string | null;
  working: boolean;
}

class CodexSession implements Session {
  readonly id: string;
  readonly cwd: string;
  private closed = false;
  private runtime: SessionRuntime;
  private normalizer = new CodexNormalizer();
  private broadcaster = new Broadcaster<SessionEvent>();
  private promptQueue: Promise<unknown> = Promise.resolve();
  private idleWaiters = new Set<() => void>();
  private onClosed?: () => void;

  constructor(
    threadId: string,
    cwd: string,
    private readonly client: JsonRpcClient,
    private readonly spawnTurn: (input: ReturnType<typeof toCodexInput>) => Promise<{ turnId: string }>,
    onClosed?: () => void,
  ) {
    this.id = threadId;
    this.cwd = cwd;
    this.runtime = { threadId, cwd, turnId: null, working: false };
    this.onClosed = onClosed;
  }

  readonly events: AsyncIterable<SessionEvent> = this.broadcaster.subscribe();

  /** driver 收到本线程的通知时调用 */
  feed(notification: JsonRpcNotification): void {
    if (this.closed) return;
    const wire = this.toWireEvent(notification);
    if (!wire) return;
    const events = this.normalizer.push(wire);
    // 更新工作状态
    const snap = this.normalizer.snapshot();
    this.runtime.working = snap.intervalActive;
    if (!this.runtime.working) this.flushIdleWaiters();
    if (wire.method === "turn/started") {
      this.runtime.turnId = wire.params.turn.id;
    }
    if (wire.method === "turn/completed") {
      this.runtime.turnId = null;
    }
    const now = Date.now();
    for (const e of events) {
      this.broadcaster.emit({ event: e, timestamp: now });
    }
  }

  private toWireEvent(n: JsonRpcNotification): CodexWireEvent | null {
    const params = n.params as Record<string, unknown> | undefined;
    if (!params || params["threadId"] !== this.id) return null;
    return { method: n.method, params } as unknown as CodexWireEvent;
  }

  private assertOpen(): void {
    if (this.closed) throw new SessionClosedError(`Session ${this.id} 已关闭`);
  }

  prompt(input: Input): Promise<void> {
    this.assertOpen();
    const blocks = toCodexInput(input);
    if (blocks.length === 0) return Promise.resolve();
    const run = this.promptQueue.then(() => this.deliverPrompt(blocks));
    this.promptQueue = run.catch(() => {});
    return run;
  }

  private async deliverPrompt(blocks: { type: "text"; text: string }[]): Promise<void> {
    this.assertOpen();
    if (!this.runtime.working) {
      // idle → 启动新工作
      const { turnId } = await this.spawnTurn(blocks);
      this.runtime.turnId = turnId;
      this.runtime.working = true;
      return;
    }
    // 忙 → steer 注入；若 steer 失败（竞态：turn 已结束），回退为启动新工作
    try {
      const params: Record<string, unknown> = {
        threadId: this.id,
        input: blocks,
      };
      if (this.runtime.turnId) params.turnId = this.runtime.turnId;
      await this.client.request("turn/steer", params);
    } catch {
      if (this.closed) throw new SessionClosedError(`Session ${this.id} 已关闭`);
      const { turnId } = await this.spawnTurn(blocks);
      this.runtime.turnId = turnId;
      this.runtime.working = true;
    }
  }

  async cancel(): Promise<void> {
    this.assertOpen();
    if (!this.runtime.working) return; // 空闲则无操作
    if (this.runtime.turnId) {
      try {
        await this.client.request("turn/interrupt", {
          threadId: this.id,
          turnId: this.runtime.turnId,
        });
      } catch {
        // interrupt 失败：等待自然结束或超时
      }
    }
    // 阻塞直到工作区间收尾（normalizer 发出 idle）
    await this.waitForIdle(30000);
    if (this.runtime.working) {
      // 超时仍无 turn/completed：手动收尾，避免会话永久卡在忙状态
      this.runtime.turnId = null;
      this.runtime.working = false;
      const now = Date.now();
      this.broadcaster.emit({
        event: { kind: "error", message: "等待工作区间收尾超时" },
        timestamp: now,
      });
      for (const e of this.normalizer.finish("cancelled")) {
        this.broadcaster.emit({ event: e, timestamp: now });
      }
      this.flushIdleWaiters();
    }
  }

  private flushIdleWaiters(): void {
    const waiters = [...this.idleWaiters];
    this.idleWaiters.clear();
    for (const w of waiters) w();
  }

  private waitForIdle(timeoutMs: number): Promise<void> {
    if (!this.runtime.working) return Promise.resolve();
    return new Promise<void>((resolve) => {
      const onIdle = () => {
        clearTimeout(timer);
        resolve();
      };
      const timer = setTimeout(() => {
        this.idleWaiters.delete(onIdle);
        resolve();
      }, timeoutMs);
      this.idleWaiters.add(onIdle);
    });
  }

  async close(): Promise<void> {
    if (this.closed) return;
    if (this.runtime.working) {
      try {
        await this.cancel();
      } catch {
        /* ignore */
      }
    }
    this.closed = true;
    this.broadcaster.close();
    this.onClosed?.();
  }
}

export class CodexDriver implements Driver {
  private client: JsonRpcClient | null = null;
  private initialized: Promise<void> | null = null;
  private sessions = new Map<string, CodexSession>();
  private stderrTail: string[] = [];

  constructor(private readonly binary: string = "codex") {}

  private async ensureServer(): Promise<JsonRpcClient> {
    if (this.client && !this.client.isClosed) {
      await this.initialized;
      return this.client;
    }
    const client = new JsonRpcClient(
      this.binary,
      ["app-server"],
      { stderr: (line) => this.stderrTail.push(line) },
    );
    this.client = client;
    this.initialized = client
      .request("initialize", { clientInfo: { name: "ahal-codex", version: "1" } })
      .then(() => undefined)
      .catch((e) => {
        this.stderrTail.push(`initialize 失败: ${e.message}`);
        throw new HarnessUnavailableError(`codex app-server 初始化失败: ${e.message}`);
      });
    client.onNotification((n) => this.dispatchNotification(n));
    await this.initialized;
    return client;
  }

  private dispatchNotification(n: JsonRpcNotification): void {
    const params = n.params as { threadId?: string } | undefined;
    if (!params?.threadId) return;
    const session = this.sessions.get(params.threadId);
    if (session) session.feed(n);
  }

  async createSession(options: SessionOptions): Promise<Session> {
    const client = await this.ensureServer();
    const result = (await client.request("thread/start", {
      cwd: options.cwd,
      ...YOLO_PARAMS,
    })) as { thread?: { id?: string } };
    const threadId = result.thread?.id;
    if (!threadId) throw new HarnessUnavailableError("thread/start 未返回 thread id");
    return this.attachSession(threadId, options.cwd, client);
  }

  async resumeSession(sessionId: string, cwd?: string): Promise<Session> {
    const client = await this.ensureServer();
    try {
      const params: Record<string, unknown> = { threadId: sessionId };
      if (cwd) params.cwd = cwd;
      const result = (await client.request("thread/resume", params)) as {
        thread?: { id?: string; cwd?: string };
      };
      const threadId = result.thread?.id ?? sessionId;
      const resumeCwd = result.thread?.cwd ?? cwd ?? process.cwd();
      return this.attachSession(threadId, resumeCwd, client);
    } catch (e) {
      throw new SessionNotFoundError(`无法恢复会话 ${sessionId}: ${(e as Error).message}`);
    }
  }

  private attachSession(threadId: string, cwd: string, client: JsonRpcClient): Session {
    const existing = this.sessions.get(threadId);
    if (existing) return existing;
    const session = new CodexSession(
      threadId,
      cwd,
      client,
      async (input) => {
        const res = (await client.request("turn/start", {
          threadId,
          cwd,
          input,
          ...YOLO_PARAMS,
        })) as { turn?: { id?: string } };
        const turnId = res.turn?.id;
        if (!turnId) throw new HarnessUnavailableError("turn/start 未返回 turn id");
        return { turnId };
      },
      () => {
        this.sessions.delete(threadId);
        this.maybeShutdown();
      },
    );
    this.sessions.set(threadId, session);
    return session;
  }

  private maybeShutdown(): void {
    if (this.sessions.size === 0 && this.client && !this.client.isClosed) {
      this.client.close();
      this.client = null;
    }
  }
}

export function createCodexDriver(options?: { binary?: string }): Driver {
  return new CodexDriver(options?.binary ?? process.env.CODEX_BINARY ?? "codex");
}

export type { CodexItem };
export type { Event };
