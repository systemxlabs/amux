/**
 * JSON-RPC 分发核心：方法注册表 + 请求/通知处理。
 * 纯逻辑，不依赖传输层；错误映射见 errors.ts。
 */

import { RpcErrorCode, type JsonRpcMessage, type JsonRpcNotification, type JsonRpcRequest, type JsonRpcResponse } from "shared";
import { toRpcError } from "./errors.js";

export type RpcHandler = (params: unknown) => Promise<unknown>;

export class RpcServer {
  private handlers = new Map<string, RpcHandler>();

  register(method: string, handler: RpcHandler): void {
    this.handlers.set(method, handler);
  }

  has(method: string): boolean {
    return this.handlers.has(method);
  }

  /** 处理一帧消息；请求返回响应（无则返回 null），通知无响应返回 null。 */
  async handle(message: JsonRpcMessage): Promise<JsonRpcResponse | null> {
    if ("method" in message) {
      if ("id" in message) return this.handleRequest(message as JsonRpcRequest);
      await this.handleNotification(message as JsonRpcNotification);
      return null;
    }
    // 响应帧：server 不会主动收到（它不是客户端），忽略
    return null;
  }

  private async handleRequest(req: JsonRpcRequest): Promise<JsonRpcResponse> {
    const handler = this.handlers.get(req.method);
    if (!handler) {
      return { jsonrpc: "2.0", id: req.id, error: { code: RpcErrorCode.MethodNotFound, message: `方法不存在: ${req.method}` } };
    }
    try {
      const result = await handler(req.params);
      return { jsonrpc: "2.0", id: req.id, result: result ?? null };
    } catch (err) {
      return { jsonrpc: "2.0", id: req.id, error: toRpcError(err) };
    }
  }

  private async handleNotification(n: JsonRpcNotification): Promise<void> {
    const handler = this.handlers.get(n.method);
    if (!handler) return; // 通知无响应：未知方法静默忽略
    try {
      await handler(n.params);
    } catch {
      // 通知的错误不回响应，只记日志（调用方处理）
      console.error(`通知处理失败: ${n.method}`);
    }
  }
}
