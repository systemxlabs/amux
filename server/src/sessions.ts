/**
 * 会话管理：创建/恢复/关闭/删除、prompt 串行化、cancel、事件管道（广播/历史落盘）。
 * 与 harness 的交互只经 ahal Driver/Session 接口（docs/AHAL.md）。
 *
 * 事件交付（docs/DESIGN.md §3.1/§5）：会话历史只落对话内容（jsonl 追加顺序），
 * 实时通知不携带序号，重连补齐由 server 按连接对齐——本类为每个事件分配内部
 * 单调序号（实现细节，不进协议），供 broadcast 层做按连接补齐位置对齐。
 */

import { randomUUID } from "node:crypto";
import { SessionClosedError, SessionNotFoundError, type Driver, type Input, type Session, type SessionEvent } from "ahal";
import { Notifications, type EventNotification, type HarnessName, type HistoryItem, type SessionMeta, type StoredEvent, type StoredUserMessage, type UserMessageNotification } from "shared";
import { HistoryStore } from "./history.js";
import { toMeta, type RegisteredSession, type SessionRegistry } from "./registry.js";

/**
 * 落盘白名单：会话历史只保存对话内容——用户输入、agent 思考、工具调用、agent 输出。
 * 状态变化、用量、错误、流式片段（*_chunk）不落盘（docs/DESIGN.md §5）。
 */
function shouldPersist(ev: { kind: string }): boolean {
  return ev.kind === "agent_message" || ev.kind === "agent_thought" || ev.kind === "tool_call_update";
}

/** 单个活跃会话：持有 ahal Session + 事件泵 + prompt 串行队列。 */
class ManagedSession {
  private queue: Promise<void> = Promise.resolve();

  constructor(
    public readonly meta: RegisteredSession,
    private readonly ah: Session,
    private readonly owner: SessionManager,
  ) {}

  /** 串行化：prompt（含用户消息记录）按调用顺序排队执行（多客户端并发也保证顺序）。 */
  prompt(input: Input): Promise<void> {
    const run = this.queue.then(async () => {
      // 用户消息记录与 ah.prompt 在同一队列内：并发 steer 时落盘/广播顺序与执行顺序一致
      await this.owner.recordUserMessage(this, input);
      await this.ah.prompt(input);
    });
    this.queue = run.then(
      () => undefined,
      () => undefined,
    );
    return run;
  }

  cancel(): Promise<void> {
    return this.ah.cancel();
  }

  close(): Promise<void> {
    return this.ah.close();
  }
}

export interface SessionManagerDeps {
  registry: SessionRegistry;
  history: HistoryStore;
  broadcast: {
    notify(name: string, params: unknown): void;
    /** 会话流通知（event / user_message）：按连接补齐路由（docs/DESIGN.md §3.1） */
    notifyStream(method: "event" | "user_message", sessionId: string, order: number, params: unknown): void;
  };
  harnesses: { createDriver(name: HarnessName): Driver };
  logger?: (line: string) => void;
}

export class SessionManager {
  private readonly live = new Map<string, ManagedSession>();
  /** 每会话内部单调序号（仅用于按连接补齐位置对齐，不进协议、不落盘）。 */
  private readonly orders = new Map<string, number>();

  constructor(private readonly deps: SessionManagerDeps) {}

  private log(line: string): void {
    this.deps.logger?.(`[sessions] ${line}`);
  }

  // ---- 查询 ----

  list(): SessionMeta[] {
    return this.deps.registry
      .all()
      .map(toMeta)
      .sort((a, b) => b.createdAt - a.createdAt);
  }

  /** 注册表中是否存在（含 closed/interrupted） */
  has(id: string): boolean {
    return this.deps.registry.has(id);
  }

  /** 是否有会话在该 cwd 上处于工作状态（git undo 需等工作区间结束）。 */
  anyBusyInCwd(cwd: string): boolean {
    for (const m of this.live.values()) {
      if (m.meta.cwd === cwd && !m.meta.closed && !m.meta.interrupted && m.meta.lastState !== "idle") return true;
    }
    return false;
  }

  historyFor(sessionId: string): HistoryItem[] {
    this.requireEntry(sessionId);
    return this.deps.history.load(sessionId);
  }

  /** 会话当前的内部序号（get_history 标记按连接补齐位置用；同步读，与历史快照原子对齐）。 */
  currentOrder(sessionId: string): number {
    return this.orders.get(sessionId) ?? 0;
  }

  private nextOrder(sessionId: string): number {
    const n = (this.orders.get(sessionId) ?? 0) + 1;
    this.orders.set(sessionId, n);
    return n;
  }

  private requireEntry(id: string): RegisteredSession {
    const e = this.deps.registry.get(id);
    if (!e) throw new SessionNotFoundError(`会话不存在: ${id}`);
    return e;
  }

  private requireLive(id: string): ManagedSession {
    const m = this.live.get(id);
    if (!m) {
      const entry = this.requireEntry(id);
      throw new SessionClosedError(entry.interrupted ? "会话已中断，请先恢复" : "会话未打开，请先恢复");
    }
    if (m.meta.closed) throw new SessionClosedError("会话已关闭");
    return m;
  }

  // ---- 生命周期 ----

  async create(params: { harness: HarnessName; cwd: string; model?: string }): Promise<SessionMeta> {
    const driver = this.deps.harnesses.createDriver(params.harness);
    const ah = await driver.createSession({ cwd: params.cwd, ...(params.model ? { model: params.model } : {}) });
    const meta: RegisteredSession = {
      id: `s_${randomUUID()}`,
      harnessSessionId: ah.id,
      harness: params.harness,
      cwd: params.cwd,
      model: params.model,
      createdAt: Date.now(),
      lastEventAt: Date.now(),
      lastState: "idle",
      closed: false,
      interrupted: false,
    };
    this.deps.registry.upsert(meta);
    this.deps.registry.save();
    this.attach(meta, ah);
    this.deps.broadcast.notify(Notifications.SessionCreated, { session: toMeta(meta) });
    return toMeta(meta);
  }

  async resume(sessionId: string): Promise<SessionMeta> {
    const entry = this.requireEntry(sessionId);
    const existing = this.live.get(sessionId);
    if (existing) return toMeta(existing.meta);
    const driver = this.deps.harnesses.createDriver(entry.harness);
    const ah = await driver.resumeSession(entry.harnessSessionId, entry.cwd);
    entry.closed = false;
    entry.interrupted = false;
    entry.lastState = "idle";
    this.deps.registry.upsert(entry);
    this.deps.registry.save();
    this.attach(entry, ah);
    this.deps.broadcast.notify(Notifications.SessionCreated, { session: toMeta(entry) });
    return toMeta(entry);
  }

  async prompt(sessionId: string, input: Input): Promise<void> {
    const managed = this.requireLive(sessionId);
    // 用户输入记录（server 自身存储，不进 AHAL 事件流）与 ah.prompt 在同一串行队列内
    await managed.prompt(input);
  }

  /**
   * 记录用户输入：写入会话历史（与事件同一 jsonl、按追加顺序）、
   * 以 user_message 通知广播（客户端据此渲染"我"的气泡，重连可补齐）。
   * 由 ManagedSession.prompt 在串行队列内调用，保证并发 steer 时顺序一致。
   */
  recordUserMessage(managed: ManagedSession, input: Input): Promise<void> {
    const entry = managed.meta;
    const now = Date.now();
    entry.lastEventAt = now;
    const rec: StoredUserMessage = { content: input, timestamp: now };
    return this.deps.history.append(entry.id, rec).then(() => {
      this.deps.registry.upsert(entry);
      this.deps.broadcast.notifyStream(Notifications.UserMessage, entry.id, this.nextOrder(entry.id), {
        sessionId: entry.id,
        content: input,
        timestamp: now,
      } satisfies UserMessageNotification);
    });
  }

  async cancel(sessionId: string): Promise<void> {
    const managed = this.requireLive(sessionId);
    await managed.cancel();
  }

  async close(sessionId: string): Promise<void> {
    const entry = this.requireEntry(sessionId);
    const managed = this.live.get(sessionId);
    if (managed) {
      try {
        await managed.close();
      } catch (e) {
        this.log(`close 失败 (${sessionId}): ${(e as Error).message}`);
      }
      this.live.delete(sessionId);
    }
    entry.closed = true;
    this.deps.registry.upsert(entry);
    this.deps.registry.save();
    this.deps.broadcast.notify(Notifications.SessionClosed, { session: toMeta(entry) });
  }

  async delete(sessionId: string): Promise<void> {
    const entry = this.requireEntry(sessionId);
    const managed = this.live.get(sessionId);
    if (managed) {
      try {
        await managed.close();
      } catch (e) {
        this.log(`close 失败 (${sessionId}): ${(e as Error).message}`);
      }
      this.live.delete(sessionId);
    }
    this.deps.registry.remove(sessionId);
    this.deps.registry.save();
    this.orders.delete(sessionId);
    await this.deps.history.remove(sessionId);
    this.deps.broadcast.notify(Notifications.SessionDeleted, { session: toMeta({ ...entry, closed: true }) });
  }

  /**
   * 重启恢复：closed 会话保持；上次忙（未正常关闭）的会话标 interrupted（客户端决定 resume）；
   * 空闲会话尝试自动恢复，失败也标 interrupted。
   */
  async restore(): Promise<void> {
    for (const entry of this.deps.registry.all()) {
      if (entry.closed) continue;
      if (entry.lastState !== "idle") {
        entry.interrupted = true;
        this.deps.registry.upsert(entry);
        continue;
      }
      try {
        const driver = this.deps.harnesses.createDriver(entry.harness);
        const ah = await driver.resumeSession(entry.harnessSessionId, entry.cwd);
        this.attach(entry, ah);
      } catch (e) {
        entry.interrupted = true;
        this.deps.registry.upsert(entry);
        this.log(`恢复会话失败 (${entry.id}): ${(e as Error).message}`);
      }
    }
    this.deps.registry.save();
  }

  /** 优雅关闭：先保存当前状态（忙的会话保持 busy → 重启后标 interrupted），再关闭全部会话。 */
  async shutdown(): Promise<void> {
    this.deps.registry.save();
    for (const [id, m] of this.live) {
      try {
        await m.close();
      } catch {
        // 忽略关闭错误
      }
      this.live.delete(id);
    }
  }

  // ---- 内部 ----

  private attach(meta: RegisteredSession, ah: Session): void {
    const managed = new ManagedSession(meta, ah, this);
    this.live.set(meta.id, managed);
    this.startPump(meta.id, ah);
  }

  private startPump(sessionId: string, ah: Session): void {
    const entry = this.deps.registry.get(sessionId);
    if (!entry) return;
    void (async () => {
      try {
        for await (const se of ah.events as AsyncIterable<SessionEvent>) {
          if (!this.deps.registry.has(sessionId)) break; // 会话已删除
          entry.lastEventAt = se.timestamp;
          const stored: StoredEvent = { event: se.event, timestamp: se.timestamp };
          // 先落盘再广播：保证 get_history 的同步读盘与补齐位置原子对齐
          if (shouldPersist(se.event)) {
            await this.deps.history.append(sessionId, stored);
          }
          if (se.event.kind === "state_changed") {
            entry.lastState = se.event.state;
            this.deps.registry.upsert(entry);
            this.deps.registry.save();
          }
          this.deps.broadcast.notifyStream(Notifications.Event, sessionId, this.nextOrder(sessionId), {
            sessionId,
            event: se.event,
            timestamp: se.timestamp,
          } satisfies EventNotification);
        }
      } catch (e) {
        this.log(`事件泵异常 (${sessionId}): ${(e as Error).message}`);
      }
    })();
  }
}
