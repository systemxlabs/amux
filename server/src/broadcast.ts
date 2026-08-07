/**
 * 广播器：向所有已连接（已认证）客户端推送 JSON-RPC 通知。
 * 连接即收流、无订阅机制、多客户端互不踢出（docs/DESIGN.md §3.1）。
 *
 * 会话流通知（event / user_message）走按连接补齐（docs/DESIGN.md §3.1/§5）：
 * 每个连接持有 ConnectionCatchup，未补齐的会话先暂存、get_history 后按序补发；
 * 其余通知（session_created 等）全量广播。
 */

import type { WebSocket } from "ws";
import type { NotificationName } from "shared";
import { ConnectionCatchup } from "./catchup.js";
import { encodeMessage } from "./jsonrpc.js";

/** 会话流通知（走按连接补齐）的方法名。 */
export type StreamNotificationName = "event" | "user_message";

export class Broadcaster {
  private readonly conns = new Map<WebSocket, ConnectionCatchup>();

  add(s: WebSocket): void {
    this.conns.set(s, new ConnectionCatchup());
  }

  remove(s: WebSocket): void {
    this.conns.get(s)?.clear();
    this.conns.delete(s);
  }

  get size(): number {
    return this.conns.size;
  }

  /** 连接对应的按连接补齐状态（get_history 处理用）。 */
  catchupFor(s: WebSocket): ConnectionCatchup | undefined {
    return this.conns.get(s);
  }

  /** 向指定连接发送一帧通知（不经过补齐逻辑；供 get_history 的缺口补发）。 */
  sendTo(s: WebSocket, method: string, params: unknown): void {
    if (s.readyState !== 1 /* OPEN */) return;
    try {
      s.send(encodeMessage({ jsonrpc: "2.0", method, params }));
    } catch {
      // 单个 socket 失败不影响其余
    }
  }

  /** 普通通知：全量广播（session_created 等生命周期通知）。 */
  notify(name: NotificationName, params: unknown): void {
    const frame = encodeMessage({ jsonrpc: "2.0", method: name, params });
    for (const s of this.conns.keys()) {
      if (s.readyState === 1 /* OPEN */) {
        try {
          s.send(frame);
        } catch {
          // 单个 socket 失败不影响其余
        }
      }
    }
  }

  /** 会话流通知（event / user_message）：按连接补齐路由（暂存 / 跳过 / 实时）。 */
  notifyStream(method: StreamNotificationName, sessionId: string, order: number, params: unknown): void {
    const frame = encodeMessage({ jsonrpc: "2.0", method, params });
    for (const [s, catchup] of this.conns) {
      if (catchup.route(sessionId, order, method, params) !== "deliver") continue;
      if (s.readyState !== 1 /* OPEN */) continue;
      try {
        s.send(frame);
      } catch {
        // 单个 socket 失败不影响其余
      }
    }
  }
}
