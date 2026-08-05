/**
 * kimi driver：spawn `kimi acp`（Agent Client Protocol over stdio）。
 *
 * yolo：session/new 携带 config.mode = "yolo"。
 * prompt：idle 启动新工作；忙时再次 session/prompt 即 steer（kimi ACP 支持注入）。
 *   kimi 的 session/prompt 响应在 turn 结束时返回（携带 stopReason）——
 *   驱动层以"收到首个本 turn 更新"作为已接受信号来 resolve prompt()。
 * cancel：session/cancel；resume：session/resume { sessionId, cwd }。
 */
import {
  InvalidInputError,
  SessionClosedError,
  SessionNotFoundError,
  type ContentBlock,
  type Driver,
  type Input,
  type Session,
  type SessionEvent,
  type SessionOptions,
} from "ahal";
import { JsonRpcClient, type JsonRpcNotification } from "./jsonrpc.js";
import { KimiNormalizer, type KimiUpdate } from "./normalize.js";

const YOLO_CONFIG = { mode: "yolo" };

/** ACP prompt 内容块（kimi acp 协议形状） */
type AcpContentBlock =
  | { type: "text"; text: string }
  | { type: "resource"; resource: { uri?: string; mimeType?: string; text?: string } };

/** AHAL 内容块 → ACP prompt 内容块 */
function toAcpPrompt(input: Input): AcpContentBlock[] {
  const out: AcpContentBlock[] = [];
  for (const block of input) {
    if (block.type === "text") {
      out.push({ type: "text", text: block.text });
    } else if (block.type === "resource" && "text" in block) {
      out.push({
        type: "resource",
        resource: { uri: block.uri, mimeType: block.mimeType, text: block.text },
      });
    } else if (block.type === "resource_link") {
      out.push({
        type: "resource",
        resource: { uri: block.uri, mimeType: block.mimeType },
      });
    } else {
      throw new InvalidInputError("二进制资源（blob）暂不支持");
    }
  }
  return out;
}

/** 极简 hot stream 广播器（与其它 driver 同构） */
class Broadcaster<T> {
  private subs = new Set<Subscriber<T>>();
  emit(item: T): void {
    for (const s of [...this.subs]) s.push(item);
  }
  subscribe(): AsyncIterable<T> {
    const sub = new Subscriber<T>(() => this.subs.delete(sub));
    this.subs.add(sub);
    return sub;
  }
  close(): void {
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

class KimiSession implements Session {
  readonly id: string;
  readonly cwd: string;
  private closed = false;
  private normalizer = new KimiNormalizer();
  private broadcaster = new Broadcaster<SessionEvent>();
  private promptQueue: Promise<unknown> = Promise.resolve();
  private pendingPrompt: { resolve: () => void; reject: (e: Error) => void } | null = null;
  private working = false;
  private onClosed?: () => void;

  constructor(
    sessionId: string,
    cwd: string,
    private readonly client: JsonRpcClient,
    private readonly sendPrompt: (prompt: AcpContentBlock[]) => Promise<{ stopReason?: string }>,
    private readonly sendCancel: () => Promise<unknown>,
    onClosed?: () => void,
  ) {
    this.id = sessionId;
    this.cwd = cwd;
    this.onClosed = onClosed;
  }

  readonly events: AsyncIterable<SessionEvent> = this.broadcaster.subscribe();

  private assertOpen(): void {
    if (this.closed) throw new SessionClosedError(`Session ${this.id} 已关闭`);
  }

  /** driver 收到本 session 的 session/update 通知时调用 */
  feed(notification: JsonRpcNotification): void {
    if (this.closed) return;
    const params = notification.params as { update?: KimiUpdate } | undefined;
    const update = params?.update;
    if (!update || typeof update.sessionUpdate !== "string") return;
    const events = this.normalizer.push(update);
    const now = Date.now();
    for (const e of events) this.broadcaster.emit({ event: e, timestamp: now });
    // 首个更新 = 已接受信号
    this.pendingPrompt?.resolve();
    this.pendingPrompt = null;
    const snap = this.normalizer.snapshot();
    this.working = snap.intervalActive;
  }

  /** prompt 完成（session/prompt 响应带 stopReason）时调用 */
  complete(stopReason?: string): void {
    if (this.closed) return;
    const events = this.normalizer.finish(stopReason);
    const now = Date.now();
    for (const e of events) this.broadcaster.emit({ event: e, timestamp: now });
    this.pendingPrompt?.resolve();
    this.pendingPrompt = null;
    this.working = this.normalizer.snapshot().intervalActive;
  }

  /** prompt 出错（error 响应 / 进程异常）时调用 */
  fail(e: Error): void {
    if (this.closed) return;
    const events = this.normalizer.push({ sessionUpdate: "error", message: e.message } as unknown as KimiUpdate);
    const now = Date.now();
    for (const ev of events) this.broadcaster.emit({ event: ev, timestamp: now });
    this.pendingPrompt?.reject(e);
    this.pendingPrompt = null;
  }

  prompt(input: Input): Promise<void> {
    this.assertOpen();
    const prompt = toAcpPrompt(input);
    if (prompt.length === 0) return Promise.resolve();
    const run = this.promptQueue.then(() => this.deliver(prompt));
    this.promptQueue = run.catch(() => {});
    return run;
  }

  private async deliver(prompt: AcpContentBlock[]): Promise<void> {
    this.assertOpen();
    this.working = true;
    await new Promise<void>((resolve, reject) => {
      this.pendingPrompt = { resolve, reject };
      this.sendPrompt(prompt)
        .then((res) => {
          // kimi 在 turn 完成时返回 stopReason → 收尾
          this.complete(res.stopReason);
        })
        .catch((e) => this.fail(e as Error));
    });
  }

  async cancel(): Promise<void> {
    this.assertOpen();
    if (!this.working) return;
    try {
      await this.sendCancel();
    } catch {
      /* ignore */
    }
    this.complete("cancelled");
  }

  async close(): Promise<void> {
    if (this.closed) return;
    if (this.working) {
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

export class KimiDriver implements Driver {
  private client: JsonRpcClient | null = null;
  private initialized: Promise<void> | null = null;
  private sessions = new Map<string, KimiSession>();

  constructor(private readonly binary: string = "kimi") {}

  private async ensureServer(): Promise<JsonRpcClient> {
    if (this.client && !this.client.isClosed) {
      await this.initialized;
      return this.client;
    }
    const client = new JsonRpcClient(this.binary, ["acp"], {
      stderr: () => {
        /* 保留 stderr 观察，不阻塞 */
      },
    });
    this.client = client;
    this.initialized = client
      .request("initialize", {
        protocolVersion: 1,
        clientInfo: { name: "ahal-kimi", version: "1" },
      })
      .then(() => undefined)
      .catch(() => {
        throw new Error("kimi acp 初始化失败（未登录或不可用）");
      });
    client.onNotification((n) => this.dispatch(n));
    await this.initialized;
    return client;
  }

  private dispatch(n: JsonRpcNotification): void {
    if (n.method !== "session/update") return;
    const params = n.params as { sessionId?: string } | undefined;
    if (!params?.sessionId) return;
    const session = this.sessions.get(params.sessionId);
    if (session) session.feed(n);
  }

  async createSession(options: SessionOptions): Promise<Session> {
    const client = await this.ensureServer();
    const result = (await client.request("session/new", {
      cwd: options.cwd,
      mcpServers: [],
      config: YOLO_CONFIG,
    })) as { sessionId?: string };
    const sessionId = result.sessionId;
    if (!sessionId) throw new Error("session/new 未返回 sessionId");
    return this.attach(sessionId, options.cwd, client);
  }

  async resumeSession(sessionId: string): Promise<Session> {
    const client = await this.ensureServer();
    try {
      const result = (await client.request("session/resume", {
        sessionId,
        cwd: process.cwd(),
      })) as { sessionId?: string };
      const sid = result.sessionId ?? sessionId;
      return this.attach(sid, process.cwd(), client);
    } catch (e) {
      throw new SessionNotFoundError(`无法恢复会话 ${sessionId}: ${(e as Error).message}`);
    }
  }

  private attach(sessionId: string, cwd: string, client: JsonRpcClient): Session {
    const existing = this.sessions.get(sessionId);
    if (existing) return existing;
    const session = new KimiSession(
      sessionId,
      cwd,
      client,
      async (prompt) => {
        const res = (await client.request("session/prompt", {
          sessionId,
          prompt,
        })) as { stopReason?: string };
        return res;
      },
      () => client.request("session/cancel", { sessionId }),
      () => {
        this.sessions.delete(sessionId);
        this.maybeShutdown();
      },
    );
    this.sessions.set(sessionId, session);
    return session;
  }

  private maybeShutdown(): void {
    if (this.sessions.size === 0 && this.client && !this.client.isClosed) {
      this.client.close();
      this.client = null;
    }
  }
}

export function createKimiDriver(options?: { binary?: string }): Driver {
  return new KimiDriver(options?.binary ?? process.env.KIMI_BINARY ?? "kimi");
}
