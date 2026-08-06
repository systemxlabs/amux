/**
 * claude driver：基于 @anthropic-ai/claude-agent-sdk 的进程内适配。
 *
 * yolo：permissionMode "bypassPermissions"（等价 --dangerously-skip-permissions）。
 * prompt：新工作 = query()；忙时 steer = abort 当前 query + 以 continue/resume 续跑。
 * cancel：abort 当前 query，随后手动收尾为 idle(cancelled)。
 * resume：query({ options: { resume: sessionId } })（~/.claude/projects 持久化）。
 */
import { getSessionInfo, query } from "@anthropic-ai/claude-agent-sdk";
import {
  AhalError,
  InvalidInputError,
  SessionClosedError,
  SessionNotFoundError,
  type Driver,
  type Input,
  type Session,
  type SessionEvent,
  type SessionOptions,
} from "ahal";
import { ClaudeNormalizer, isBusyMessage, type ClaudeMessage } from "./normalize.js";

const PERMISSION_MODE = "bypassPermissions";

function toPromptText(input: Input): string {
  const parts: string[] = [];
  for (const block of input) {
    if (block.type === "text") parts.push(block.text);
    else if (block.type === "resource" && "text" in block) parts.push(block.text);
    else if (block.type === "resource_link") parts.push(block.uri);
    else throw new InvalidInputError("二进制资源（blob）暂不支持");
  }
  return parts.join("\n");
}

/** 极简 hot stream 广播器（与 ahal-codex 同构） */
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

class ClaudeSession implements Session {
  private _id: string;
  readonly cwd: string;
  private closed = false;
  private working = false;
  private sessionId: string | null;
  private normalizer = new ClaudeNormalizer();
  private broadcaster = new Broadcaster<SessionEvent>();
  private abortCtrl: AbortController | null = null;
  private promptQueue: Promise<unknown> = Promise.resolve();
  /** cancel 代数：steer 打断等待期间发生 cancel 时递增，用于放弃被并发取消的注入 */
  private generation = 0;

  constructor(
    private readonly opts: SessionOptions,
    private readonly resumeId: string | null,
  ) {
    this.cwd = opts.cwd;
    this._id = resumeId ?? `claude-session-${Math.random().toString(36).slice(2, 10)}`;
    this.sessionId = resumeId;
  }

  /** AHAL Session id：新建会话在首次 query 后更新为 SDK 真实 session_id */
  get id(): string {
    return this._id;
  }

  readonly events: AsyncIterable<SessionEvent> = this.broadcaster.subscribe();

  private assertOpen(): void {
    if (this.closed) throw new SessionClosedError(`Session ${this.id} 已关闭`);
  }

  prompt(input: Input): Promise<void> {
    this.assertOpen();
    const text = toPromptText(input);
    if (!text.trim()) return Promise.resolve();
    const run = this.promptQueue.then(() => this.deliver(text));
    this.promptQueue = run.catch(() => {});
    return run;
  }

  private async deliver(text: string): Promise<void> {
    this.assertOpen();
    if (this.working) {
      // 忙 → steer：打断当前 query，续跑
      const gen = this.generation;
      this.abortCtrl?.abort();
      await this.waitWorkingDone();
      if (this.closed) throw new SessionClosedError(`Session ${this.id} 已关闭`);
      // 打断等待期间发生了 cancel：工作已被取消，不能假装消息已送达
      if (this.generation !== gen) {
        throw new AhalError("prompt 被并发 cancel 取消，未送达");
      }
    }
    await this.runQuery(text);
  }

  private async runQuery(text: string): Promise<void> {
    this.working = true;
    const controller = new AbortController();
    this.abortCtrl = controller;
    const options: Record<string, unknown> = {
      cwd: this.cwd,
      permissionMode: PERMISSION_MODE,
      abortController: controller,
      maxTurns: 200,
    };
    if (this.opts.model) options.model = this.opts.model;
    // 新建会话不带 resume/continue（避免继续 cwd 里最近的会话）；已有会话用 resume 固定
    if (this.sessionId) options.resume = this.sessionId;

    // prompt() 在首个忙消息（已接受）或 result 时 resolve；流在后台持续投递
    let resolveFirst!: () => void;
    let rejectFirst!: (e: Error) => void;
    const firstEvent = new Promise<void>((resolve, reject) => {
      resolveFirst = resolve;
      rejectFirst = reject;
    });
    let firstDone = false;

    const stream = query({ prompt: text, options: options as never });
    void (async () => {
      try {
        for await (const msg of stream as AsyncIterable<ClaudeMessage>) {
          if (msg.session_id) {
            this.sessionId = msg.session_id;
            if (!this.resumeId) this._id = msg.session_id;
          }
          const events = this.normalizer.push(msg);
          const now = Date.now();
          for (const e of events) this.broadcaster.emit({ event: e, timestamp: now });
          if (!firstDone && (isBusyMessage(msg) || msg.type === "result")) {
            firstDone = true;
            resolveFirst();
          }
          if (msg.type === "result") break;
        }
        if (!firstDone) {
          firstDone = true;
          resolveFirst();
        }
      } catch (e) {
        if (!firstDone) {
          firstDone = true;
          rejectFirst(e instanceof Error ? e : new Error(String(e)));
        }
        if (!controller.signal.aborted) {
          const message = (e as Error)?.message ?? "claude query 失败";
          const events = this.normalizer.push({
            type: "result",
            subtype: "error",
            error: message,
          } as ClaudeMessage);
          const now = Date.now();
          for (const ev of events) this.broadcaster.emit({ event: ev, timestamp: now });
        }
      } finally {
        this.working = false;
        this.abortCtrl = null;
      }
    })();

    await firstEvent;
  }

  private async waitWorkingDone(): Promise<void> {
    while (this.working) {
      await new Promise((r) => setTimeout(r, 20));
    }
  }

  async cancel(): Promise<void> {
    this.assertOpen();
    if (!this.working) return;
    this.generation++;
    this.abortCtrl?.abort();
    await this.waitWorkingDone();
    // abort 无 result，手动收尾为 idle(cancelled)
    const events = this.normalizer.finish("cancelled");
    const now = Date.now();
    for (const e of events) this.broadcaster.emit({ event: e, timestamp: now });
  }

  async close(): Promise<void> {
    if (this.closed) return;
    if (this.working) {
      this.generation++;
      this.abortCtrl?.abort();
      await this.waitWorkingDone();
    }
    this.closed = true;
    this.broadcaster.close();
  }
}

export class ClaudeDriver implements Driver {
  async createSession(options: SessionOptions): Promise<Session> {
    return new ClaudeSession(options, null);
  }

  async resumeSession(sessionId: string, cwd?: string): Promise<Session> {
    // 预检：会话不存在则立即报错（而非延迟到 prompt）
    const info = await getSessionInfo(sessionId, { dir: cwd }).catch(() => undefined);
    if (!info) {
      throw new SessionNotFoundError(`无法恢复会话 ${sessionId}: 会话不存在`);
    }
    const resumeCwd = info.cwd ?? cwd ?? process.cwd();
    return new ClaudeSession({ cwd: resumeCwd }, sessionId);
  }
}

export function createClaudeDriver(): Driver {
  return new ClaudeDriver();
}
