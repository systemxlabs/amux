/**
 * 广播器：向所有已连接（已认证）客户端推送 JSON-RPC 通知。
 * 连接即收流、无订阅机制、多客户端互不踢出（docs/DESIGN.md §3.1）。
 */

import type { WebSocket } from "ws";
import type { NotificationName } from "shared";
import { encodeMessage } from "./jsonrpc.js";

export class Broadcaster {
  private readonly sockets = new Set<WebSocket>();

  add(s: WebSocket): void {
    this.sockets.add(s);
  }

  remove(s: WebSocket): void {
    this.sockets.delete(s);
  }

  get size(): number {
    return this.sockets.size;
  }

  notify(name: NotificationName, params: unknown): void {
    const frame = encodeMessage({ jsonrpc: "2.0", method: name, params });
    for (const s of this.sockets) {
      if (s.readyState === 1 /* OPEN */) {
        try {
          s.send(frame);
        } catch {
          // 单个 socket 失败不影响其余
        }
      }
    }
  }
}
