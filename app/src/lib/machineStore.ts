/**
 * 单机器控制器：持有 MachineClient，维护会话列表与每会话事件流（SessionFeed）。
 * 事件交付按 docs/DESIGN.md §3.1/§5：重连先拉历史（get_history，按 jsonl 顺序），
 * 之后的实时/补齐项由 server 按连接对齐、以通知按序送达——客户端按序追加即可，
 * 无需 seq 去重、无需本地暂存。
 */

import {
  Methods,
  Notifications,
  type EventNotification,
  type MachineInfo,
  type SessionMeta,
  type SessionNotification,
  type UserMessageNotification,
} from "shared";
import type { Input } from "ahal";
import type { MachineConfig } from "./configStore.js";
import type { ConnectionStatus } from "./machineClient.js";
import { MachineClient } from "./machineClient.js";
import { SessionFeed, type FeedItem } from "./sessionFeed.js";

export interface MachineState {
  config: MachineConfig;
  status: ConnectionStatus;
  info: MachineInfo | null;
  sessions: SessionMeta[];
  feeds: Map<string, SessionFeed>;
  error: string | null;
}

export class MachineStore {
  readonly state: MachineState;
  private readonly client: MachineClient;
  private readonly listeners = new Set<() => void>();
  private destroyed = false;

  constructor(config: MachineConfig, opts: { setTimeout?: typeof window.setTimeout } = {}) {
    this.state = { config, status: "disconnected", info: null, sessions: [], feeds: new Map(), error: null };
    const url = new URL(config.url);
    url.searchParams.set("token", config.token);
    this.client = new MachineClient(
      url.toString(),
      {
        onStatus: (s) => {
          this.state.status = s;
          if (s === "connected") void this.onConnected();
          this.emit();
        },
        onNotification: (method, params) => this.onNotification(method, params),
      },
      opts,
    );
  }

  subscribe(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  connect(): void {
    this.client.connect();
  }

  disconnect(): void {
    this.destroyed = true;
    this.client.disconnect();
  }

  // ---- RPC 透传 ----

  request<T = unknown>(method: string, params?: unknown): Promise<T> {
    return this.client.request<T>(method, params);
  }

  async createSession(harness: string, cwd: string, model?: string): Promise<SessionMeta> {
    const res = await this.request<{ session: SessionMeta }>(Methods.CreateSession, { harness, cwd, ...(model ? { model } : {}) });
    return res.session;
  }

  async resumeSession(sessionId: string): Promise<void> {
    await this.request(Methods.ResumeSession, { sessionId });
  }

  async closeSession(sessionId: string): Promise<void> {
    await this.request(Methods.CloseSession, { sessionId });
  }

  async deleteSession(sessionId: string): Promise<void> {
    await this.request(Methods.DeleteSession, { sessionId });
  }

  async prompt(sessionId: string, input: Input): Promise<void> {
    await this.request(Methods.Prompt, { sessionId, input });
  }

  async cancel(sessionId: string): Promise<void> {
    await this.request(Methods.Cancel, { sessionId });
  }

  async gitPush(cwd: string): Promise<{ ok: boolean; message?: string }> {
    return this.request(Methods.GitPush, { cwd });
  }

  async gitRevert(cwd: string, opts: { path?: string; patch?: string } = {}): Promise<{ ok: boolean; message?: string }> {
    return this.request(Methods.GitRevert, { cwd, ...opts });
  }

  async gitStatus(cwd: string): Promise<unknown> {
    return this.request(Methods.GitStatus, { cwd });
  }

  async gitDiff(cwd: string, path?: string): Promise<string> {
    const res = await this.request<{ diff: string }>(Methods.GitDiff, { cwd, ...(path ? { path } : {}) });
    return res.diff;
  }

  // ---- 通知处理 ----

  private onNotification(method: string, params: unknown): void {
    if (this.destroyed) return;
    switch (method) {
      case Notifications.Event: {
        const n = params as EventNotification;
        this.routeLive(n.sessionId, { event: n.event, timestamp: n.timestamp });
        break;
      }
      case Notifications.UserMessage: {
        const n = params as UserMessageNotification;
        this.routeLive(n.sessionId, { content: n.content, timestamp: n.timestamp });
        break;
      }
      case Notifications.SessionCreated:
      case Notifications.SessionClosed:
      case Notifications.SessionInterrupted: {
        const n = params as SessionNotification;
        this.upsertSessionMeta(n.session);
        if (method === Notifications.SessionCreated && !this.state.feeds.has(n.session.id)) {
          void this.catchUpSession(n.session.id);
        }
        this.emit();
        break;
      }
      case Notifications.SessionDeleted: {
        const n = params as SessionNotification;
        // 永久删除：从列表与事件流移除（不可恢复，不再 upsert）
        this.state.sessions = this.state.sessions.filter((s) => s.id !== n.session.id);
        this.state.feeds.delete(n.session.id);
        this.emit();
        break;
      }
      default:
        break;
    }
  }

  /** 实时项路由：feed 已存在则按序追加；未 get_history 的会话 server 侧会暂存，不会收到通知。 */
  private routeLive(sessionId: string, item: FeedItem): void {
    const feed = this.state.feeds.get(sessionId);
    if (feed) {
      feed.applyOne(item);
    }
    // 状态变化不落历史：实时同步到会话 meta，保证头部/列表状态始终最新
    if ("event" in item && item.event.kind === "state_changed") {
      const meta = this.state.sessions.find((s) => s.id === sessionId);
      if (meta) meta.state = item.event.state;
    }
    this.emit();
  }

  private upsertSessionMeta(meta: SessionMeta): void {
    const i = this.state.sessions.findIndex((s) => s.id === meta.id);
    if (i === -1) {
      this.state.sessions.push(meta);
    } else {
      this.state.sessions[i] = meta;
    }
    this.state.sessions.sort((a, b) => b.createdAt - a.createdAt);
  }

  // ---- 重连补齐 ----

  private async onConnected(): Promise<void> {
    this.state.error = null;
    try {
      const info = await this.client.request<{ info: MachineInfo }>(Methods.GetInfo);
      this.state.info = info.info;
      const res = await this.client.request<{ sessions: SessionMeta[] }>(Methods.ListSessions);
      this.state.sessions = res.sessions.sort((a, b) => b.createdAt - a.createdAt);
      // 逐会话拉历史；补齐缺口由 server 按连接对齐，以通知按序送达（无需本地暂存）
      await Promise.all(res.sessions.map((s) => this.catchUpSession(s.id)));
    } catch (e) {
      this.state.error = (e as Error).message;
    } finally {
      this.emit();
    }
  }

  private async catchUpSession(sessionId: string): Promise<void> {
    const feed = new SessionFeed(sessionId);
    try {
      const hist = await this.client.request<{ items: FeedItem[] }>(Methods.GetHistory, { sessionId });
      feed.applyHistory(hist.items);
    } catch {
      // 会话可能刚被删除：保留空 feed
    }
    this.state.feeds.set(sessionId, feed);
    this.emit();
  }

  private emit(): void {
    for (const fn of this.listeners) fn();
  }
}
