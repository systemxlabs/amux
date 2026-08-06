/**
 * 单机器 WebSocket 协议客户端：JSON-RPC 2.0 请求/通知、指数退避重连、事件路由。
 * 协议类型来自 shared（app↔server 的唯一协议源）。
 */

import { nextBackoffDelay, type BackoffConfig } from "./backoff.js";

export type ConnectionStatus = "disconnected" | "connecting" | "connected" | "auth-error";

export interface MachineCallbacks {
  onStatus: (status: ConnectionStatus) => void;
  /** server → client 的通知（方法名 + 参数） */
  onNotification: (method: string, params: unknown) => void;
}

const DEFAULT_BACKOFF: BackoffConfig = { baseMs: 500, maxMs: 30000, factor: 2, jitter: 0.15 };

export class MachineClient {
  private ws: WebSocket | null = null;
  private status: ConnectionStatus = "disconnected";
  private attempt = 0;
  private reconnectTimer: number | null = null;
  private closedByUser = false;
  private nextId = 1;
  private pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();

  constructor(
    private readonly url: string,
    private readonly callbacks: MachineCallbacks,
    private readonly opts: { backoff?: BackoffConfig; setTimeout?: typeof window.setTimeout } = {},
  ) {}

  get currentStatus(): ConnectionStatus {
    return this.status;
  }

  connect(): void {
    this.closedByUser = false;
    if (this.ws) return;
    this.open();
  }

  disconnect(): void {
    this.closedByUser = true;
    if (this.reconnectTimer !== null) {
      this.clearTimer(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.ws?.close();
    this.ws = null;
    this.setStatus("disconnected");
    this.rejectAll(new Error("连接已关闭"));
  }

  /** 发起 JSON-RPC 请求；返回与 id 匹配的响应（result 或抛错）。 */
  request<T = unknown>(method: string, params?: unknown): Promise<T> {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error("未连接"));
    }
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject });
      this.ws!.send(JSON.stringify({ jsonrpc: "2.0", id, method, ...(params !== undefined ? { params } : {}) }));
    });
  }

  private open(): void {
    this.setStatus("connecting");
    let ws: WebSocket;
    try {
      ws = new WebSocket(this.url);
    } catch (e) {
      this.scheduleReconnect();
      return;
    }
    this.ws = ws;
    ws.onopen = () => {
      if (this.ws !== ws) return;
      this.attempt = 0;
      this.setStatus("connected");
    };
    ws.onmessage = (ev) => this.handleMessage(String(ev.data));
    ws.onclose = (ev) => {
      if (this.ws !== ws) return;
      this.ws = null;
      this.rejectAll(new Error("连接中断"));
      if (ev.code === 4401) {
        // 认证失败：不重连（token 错误需要用户处理）
        this.setStatus("auth-error");
        return;
      }
      this.setStatus("disconnected");
      if (!this.closedByUser) this.scheduleReconnect();
    };
    ws.onerror = () => {
      // close 事件兜底处理
    };
  }

  private scheduleReconnect(): void {
    if (this.closedByUser || this.reconnectTimer !== null) return;
    const delay = nextBackoffDelay(this.attempt, this.opts.backoff ?? DEFAULT_BACKOFF);
    this.attempt++;
    this.reconnectTimer = this.setTimer(() => {
      this.reconnectTimer = null;
      if (!this.closedByUser) this.open();
    }, delay);
  }

  private handleMessage(text: string): void {
    let msg: { id?: unknown; method?: unknown; params?: unknown; result?: unknown; error?: { code: number; message: string } };
    try {
      msg = JSON.parse(text);
    } catch {
      return;
    }
    if (typeof msg.method === "string") {
      // 通知（无 id）
      this.callbacks.onNotification(msg.method, msg.params);
      return;
    }
    if (typeof msg.id === "number") {
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      if (msg.error !== undefined) {
        const err = new Error(msg.error.message || `JSON-RPC 错误 ${msg.error.code}`);
        (err as Error & { code?: number }).code = msg.error.code;
        p.reject(err);
      } else {
        p.resolve(msg.result);
      }
    }
  }

  private setStatus(s: ConnectionStatus): void {
    if (this.status !== s) {
      this.status = s;
      this.callbacks.onStatus(s);
    }
  }

  private rejectAll(e: Error): void {
    for (const [, p] of this.pending) p.reject(e);
    this.pending.clear();
  }

  private setTimer(fn: () => void, ms: number): number {
    return (this.opts.setTimeout ?? window.setTimeout)(fn, ms);
  }

  private clearTimer(id: number): void {
    window.clearTimeout(id);
  }
}

export interface RpcError extends Error {
  code?: number;
}
