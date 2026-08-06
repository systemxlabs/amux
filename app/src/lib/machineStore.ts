/**
 * 单机器控制器：持有 MachineClient，维护会话列表与每会话事件流（SessionFeed），
 * 处理重连补齐（历史 + 缓冲 → 实时，seq 去重）与会话生命周期通知。
 */

import {
  Methods,
  Notifications,
  type EventNotification,
  type MachineInfo,
  type SessionMeta,
  type SessionNotification,
} from "shared";
import type { Input } from "ahal";
import type { MachineConfig } from "./configStore.js";
import type { ConnectionStatus } from "./machineClient.js";
import { MachineClient } from "./machineClient.js";
import { SessionFeed, type FeedEvent } from "./sessionFeed.js";

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
  /** 重连补齐期间到达的实时事件（等补齐完成后冲入对应 feed） */
  private pendingLive = new Map<string, EventNotification[]>();
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
        const feed = this.state.feeds.get(n.sessionId);
        if (feed) {
          feed.applyOne({ seq: n.seq, event: n.event, timestamp: n.timestamp });
        } else {
          // 补齐期间：暂存，补齐完成后冲入
          const list = this.pendingLive.get(n.sessionId) ?? [];
          list.push(n);
          this.pendingLive.set(n.sessionId, list);
        }
        this.emit();
        break;
      }
      case Notifications.SessionCreated:
      case Notifications.SessionClosed:
      case Notifications.SessionInterrupted:
      case Notifications.SessionDeleted: {
        const n = params as SessionNotification;
        this.upsertSessionMeta(n.session);
        if (method === Notifications.SessionCreated && !this.state.feeds.has(n.session.id)) {
          void this.catchUpSession(n.session.id);
        }
        if (method === Notifications.SessionDeleted) {
          this.state.feeds.delete(n.session.id);
        }
        this.emit();
        break;
      }
      default:
        break;
    }
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
      // 逐会话拉历史 + 缓冲补齐
      await Promise.all(res.sessions.map((s) => this.catchUpSession(s.id)));
      // 补齐期间暂存的实时事件：冲入 feed（seq 去重由 feed 保证）
      for (const [sid, list] of this.pendingLive) {
        const feed = this.state.feeds.get(sid);
        if (feed) {
          for (const n of list) feed.applyOne({ seq: n.seq, event: n.event, timestamp: n.timestamp });
          this.state.feeds.set(sid, feed);
        }
      }
      this.pendingLive.clear();
    } catch (e) {
      this.state.error = (e as Error).message;
    } finally {
      this.emit();
    }
  }

  private async catchUpSession(sessionId: string): Promise<void> {
    const feed = new SessionFeed(sessionId);
    try {
      const hist = await this.client.request<{ events: FeedEvent[] }>(Methods.GetHistory, { sessionId });
      feed.applyHistory(hist.events);
      const buf = await this.client.request<{ events: FeedEvent[] }>(Methods.GetBufferedEvents, {
        sessionId,
        afterSeq: feed.lastSeq,
      });
      feed.apply(buf.events);
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
