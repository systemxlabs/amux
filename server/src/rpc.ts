/**
 * JSON-RPC 分发核心：方法注册表 + 请求/通知处理。
 * 纯逻辑，不依赖传输层；错误映射见 errors.ts。
 * handler 携带连接上下文（RpcContext），供按连接补齐（get_history 标记位置/补发缺口）。
 */

import { RpcErrorCode, type JsonRpcMessage, type JsonRpcNotification, type JsonRpcRequest, type JsonRpcResponse } from "shared";
import type { ConnectionCatchup } from "./catchup.js";
import { toRpcError } from "./errors.js";

/** handler 的连接上下文（server 按连接对齐，docs/DESIGN.md §3.1）。 */
export interface RpcContext {
  /** 当前连接的按连接补齐状态 */
  catchup: ConnectionCatchup;
  /** 向当前连接发送一帧通知（get_history 的缺口补发用，须在响应发送之后调用） */
  send(method: string, params: unknown): void;
}

export interface RpcOutcome {
  result: unknown;
  /** 响应发送后执行（用于缺口补发：保证历史先于补齐项到达客户端） */
  afterSend?: () => void;
}

export type RpcHandler = (params: unknown, ctx?: RpcContext) => Promise<RpcOutcome>;

export interface HandleResult {
  response: JsonRpcResponse | null;
  afterSend?: () => void;
}

export class RpcServer {
  private handlers = new Map<string, RpcHandler>();

  register(method: string, handler: RpcHandler): void {
    this.handlers.set(method, handler);
  }

  has(method: string): boolean {
    return this.handlers.has(method);
  }

  /** 处理一帧消息；请求返回响应（无则 null），通知无响应返回 null。 */
  async handle(message: JsonRpcMessage, ctx?: RpcContext): Promise<HandleResult> {
    if ("method" in message) {
      if ("id" in message) return this.handleRequest(message as JsonRpcRequest, ctx);
      await this.handleNotification(message as JsonRpcNotification, ctx);
      return { response: null };
    }
    // 响应帧：server 不会主动收到（它不是客户端），忽略
    return { response: null };
  }

  private async handleRequest(req: JsonRpcRequest, ctx?: RpcContext): Promise<HandleResult> {
    const handler = this.handlers.get(req.method);
    if (!handler) {
      return { response: { jsonrpc: "2.0", id: req.id, error: { code: RpcErrorCode.MethodNotFound, message: `方法不存在: ${req.method}` } } };
    }
    try {
      const outcome = await handler(req.params, ctx);
      return { response: { jsonrpc: "2.0", id: req.id, result: outcome.result ?? null }, afterSend: outcome.afterSend };
    } catch (err) {
      return { response: { jsonrpc: "2.0", id: req.id, error: toRpcError(err) } };
    }
  }

  private async handleNotification(n: JsonRpcNotification, ctx?: RpcContext): Promise<void> {
    const handler = this.handlers.get(n.method);
    if (!handler) return; // 通知无响应：未知方法静默忽略
    try {
      await handler(n.params, ctx);
    } catch {
      // 通知的错误不回响应，只记日志（调用方处理）
      console.error(`通知处理失败: ${n.method}`);
    }
  }
}
