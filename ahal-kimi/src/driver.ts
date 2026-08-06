/**
 * kimi driver：基于 @botiverse/kimi-code-sdk（@moonshot-ai/kimi-code-sdk 的社区镜像，进程内封装）。
 *
 * yolo：createSession 携带 permission: "yolo"。
 * prompt：session.prompt()（忙时 session.steer()）；SDK 的 prompt 在接受时 resolve（约 16ms）。
 * cancel：session.cancel()；若收不到 turn.ended 则手动收尾 idle(cancelled)。
 * resume：harness.resumeSession({ id })（会话持久化在 ~/.kimi-code/sessions）。
 * 注意：harness 需 homeDir 指向 kimi 配置目录（~/.kimi-code），session 需先 init()。
 */
import {
  createKimiHarness,
  type Event as KimiSdkWireEvent,
  type KimiHarness,
  type Session as KimiSdkSession,
} from "@botiverse/kimi-code-sdk";
import * as os from "node:os";
import * as path from "node:path";
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
import { KimiSdkNormalizer, type KimiSdkEvent } from "./normalize.js";

function kimiHomeDir(): string {
  return process.env.KIMI_HOME ?? path.join(os.homedir(), ".kimi-code");
}

/** AHAL 内容块 → 提示文本 */
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
  private working = false;
  private normalizer = new KimiSdkNormalizer();
  private broadcaster = new Broadcaster<SessionEvent>();
  private promptQueue: Promise<unknown> = Promise.resolve();
  private onClosed?: () => void;

  constructor(
    private readonly sdk: KimiSdkSession,
    private readonly harness: KimiHarness,
    onClosed?: () => void,
  ) {
    this.id = sdk.id;
    this.cwd = sdk.workDir;
    this.onClosed = onClosed;
    // 事件订阅：先于首个 prompt 建立
    sdk.onEvent((event: KimiSdkWireEvent) => {
      if (this.closed) return;
      const events = this.normalizer.push(event as unknown as KimiSdkEvent);
      const now = Date.now();
      for (const e of events) this.broadcaster.emit({ event: e, timestamp: now });
      this.working = this.normalizer.snapshot().intervalActive;
    });
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
      await this.sdk.steer(text);
    } else {
      await this.sdk.prompt(text);
    }
  }

  async cancel(): Promise<void> {
    this.assertOpen();
    if (!this.working) return;
    try {
      await this.sdk.cancel();
    } catch {
      /* ignore */
    }
    // 等待 turn.ended(cancelled) 收尾；未到达则手动收尾
    await this.waitForIdle(15000);
    const events = this.normalizer.finish("cancelled");
    const now = Date.now();
    for (const e of events) this.broadcaster.emit({ event: e, timestamp: now });
  }

  private async waitForIdle(timeoutMs: number): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline && this.working) {
      await new Promise((r) => setTimeout(r, 20));
    }
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
    try {
      await this.harness.closeSession(this.id);
    } catch {
      /* ignore */
    }
    this.broadcaster.close();
    this.onClosed?.();
  }
}

export class KimiDriver implements Driver {
  private harness: KimiHarness | null = null;

  private async ensureHarness(): Promise<KimiHarness> {
    if (!this.harness) {
      this.harness = createKimiHarness({
        homeDir: kimiHomeDir(),
        uiMode: "headless",
        autoLoadConfig: true,
      });
    }
    return this.harness;
  }

  async createSession(options: SessionOptions): Promise<Session> {
    const harness = await this.ensureHarness();
    const sdk = await harness.createSession({
      workDir: options.cwd,
      permission: "yolo",
      ...(options.model ? { model: options.model } : {}),
    });
    await sdk.init();
    return new KimiSession(sdk, harness);
  }

  async resumeSession(sessionId: string, _cwd?: string): Promise<Session> {
    const harness = await this.ensureHarness();
    try {
      const sdk = await harness.resumeSession({ id: sessionId });
      await sdk.init();
      return new KimiSession(sdk, harness);
    } catch (e) {
      throw new SessionNotFoundError(`无法恢复会话 ${sessionId}: ${(e as Error).message}`);
    }
  }
}

export function createKimiDriver(): Driver {
  return new KimiDriver();
}

export type { ContentBlock };
