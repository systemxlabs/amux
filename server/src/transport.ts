/**
 * WebSocket 传输层（ws）：认证、连接生命周期、消息收发。
 * token 经查询参数携带（浏览器 WebSocket 无法自定义头）；认证失败以 4401 关闭。
 */

import type { IncomingMessage } from "node:http";
import { URL } from "node:url";
import { WebSocketServer, type WebSocket } from "ws";

export interface TransportOptions {
  host: string;
  port: number;
  token: string;
  onConnection: (socket: WebSocket) => void;
  onMessage: (socket: WebSocket, text: string) => void;
  onClose: (socket: WebSocket) => void;
  logger?: (line: string) => void;
}

export class Transport {
  private wss: WebSocketServer | null = null;

  constructor(private readonly opts: TransportOptions) {}

  start(): Promise<void> {
    return new Promise((resolve, reject) => {
      const wss = new WebSocketServer({ host: this.opts.host, port: this.opts.port });
      this.wss = wss;
      wss.on("error", (e) => {
        this.opts.logger?.(`ws 错误: ${e.message}`);
        reject(e);
      });
      wss.on("listening", () => resolve());
      wss.on("connection", (socket, req) => {
        if (!this.authorized(req)) {
          socket.close(4401, "unauthorized");
          return;
        }
        this.opts.onConnection(socket);
        socket.on("message", (data) => {
          this.opts.onMessage(socket, data.toString());
        });
        socket.on("close", () => this.opts.onClose(socket));
        socket.on("error", () => {
          // 连接级错误由 close 兜底
        });
      });
    });
  }

  private authorized(req: IncomingMessage): boolean {
    const url = new URL(req.url ?? "/", `http://${req.headers.host ?? "localhost"}`);
    return url.searchParams.get("token") === this.opts.token;
  }

  address(): { host: string; port: number } {
    const a = this.wss?.address();
    if (a && typeof a === "object") return { host: a.address, port: a.port };
    return { host: this.opts.host, port: this.opts.port };
  }

  close(): Promise<void> {
    return new Promise((resolve) => {
      if (!this.wss) return resolve();
      for (const c of this.wss.clients) c.close();
      this.wss.close(() => resolve());
    });
  }
}
