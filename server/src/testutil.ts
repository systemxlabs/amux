/**
 * server 单测工具：ahal Driver/Session 桩、假广播、临时目录。
 * 桩只替换 harness 侧（docs/AHAL.md 接口），server 自身代码全部为真实实现。
 */

import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { HarnessUnavailableError, SessionNotFoundError, type Driver, type Event, type Input, type Session, type SessionEvent, type SessionOptions } from "ahal";

export function tmpDir(prefix: string): string {
  return mkdtempSync(join(tmpdir(), prefix));
}

export class StubSession implements Session {
  readonly id: string;
  readonly cwd: string;
  promptCalls: Input[] = [];
  promptStarted = 0;
  promptEnded = 0;
  cancelCalls = 0;
  closed = false;
  /** 测试可替换为未 resolve 的 promise，以观察串行化行为 */
  promptGate: Promise<void> = Promise.resolve();
  private queue: SessionEvent[] = [];
  private waiters: Array<() => void> = [];

  constructor(id: string, cwd: string) {
    this.id = id;
    this.cwd = cwd;
  }

  async prompt(input: Input): Promise<void> {
    this.promptCalls.push(input);
    this.promptStarted++;
    await this.promptGate;
    await new Promise((r) => setTimeout(r, 3));
    this.promptEnded++;
  }

  async cancel(): Promise<void> {
    this.cancelCalls++;
  }

  async close(): Promise<void> {
    this.closed = true;
  }

  pushEvent(event: Event, timestamp = Date.now()): void {
    this.queue.push({ event, timestamp });
    this.waiters.shift()?.();
  }

  readonly events: AsyncIterable<SessionEvent> = {
    [Symbol.asyncIterator]: () => {
      let i = 0;
      return {
        next: async (): Promise<IteratorResult<SessionEvent>> => {
          while (i >= this.queue.length) {
            if (this.closed) return { done: true, value: undefined };
            await new Promise<void>((resolve) => this.waiters.push(resolve));
          }
          return { done: false, value: this.queue[i++] };
        },
      };
    },
  };
}

export interface StubDriverOptions {
  failCreate?: boolean;
  failResume?: boolean;
  sessions?: Map<string, StubSession>;
}

export class StubDriver implements Driver {
  constructor(private readonly opts: StubDriverOptions = {}) {}

  async createSession(options: SessionOptions): Promise<Session> {
    if (this.opts.failCreate) throw new HarnessUnavailableError("harness 未安装");
    const s = new StubSession(`s_test_${Math.random().toString(36).slice(2)}`, options.cwd);
    this.opts.sessions?.set(s.id, s);
    return s;
  }

  async resumeSession(sessionId: string, cwd?: string): Promise<Session> {
    if (this.opts.failResume) throw new SessionNotFoundError(`无法恢复会话 ${sessionId}`);
    const s = new StubSession(sessionId, cwd ?? "/tmp");
    this.opts.sessions?.set(sessionId, s);
    return s;
  }
}

/** 满足 SessionManagerDeps.harnesses 的注册表 seam。 */
export class FakeHarnessRegistry {
  constructor(private readonly drivers = new Map<string, Driver>()) {}

  createDriver(name: string): Driver {
    const d = this.drivers.get(name);
    if (!d) throw new Error(`未知 harness: ${name}`);
    return d;
  }
}

export class FakeBroadcaster {
  notifications: Array<{ method: string; params: unknown }> = [];

  notify(method: string, params: unknown): void {
    this.notifications.push({ method, params });
  }

  notifyStream(method: "event" | "user_message", _sessionId: string, _order: number, params: unknown): void {
    this.notifications.push({ method, params });
  }

  last(): { method: string; params: unknown } | undefined {
    return this.notifications[this.notifications.length - 1];
  }
}
