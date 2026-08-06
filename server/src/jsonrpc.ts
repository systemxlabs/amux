/**
 * JSON-RPC 2.0 编解码（纯函数）。
 * 协议面类型来自 shared；本文件只负责帧级解析/序列化与畸形帧判定。
 */

import { RpcErrorCode, type JsonRpcError, type JsonRpcMessage, type JsonRpcNotification, type JsonRpcRequest, type JsonRpcResponse } from "shared";

export type DecodedFrame =
  | { kind: "request"; request: JsonRpcRequest }
  | { kind: "notification"; notification: JsonRpcNotification }
  | { kind: "response"; response: JsonRpcResponse }
  | { kind: "error"; error: JsonRpcError };

/** 解析一帧文本。畸形请求返回 error 帧（由调用方以 id:null 回应）。 */
export function parseJsonRpc(text: string): DecodedFrame {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return { kind: "error", error: { code: RpcErrorCode.ParseError, message: "Parse error" } };
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return { kind: "error", error: { code: RpcErrorCode.InvalidRequest, message: "Invalid Request" } };
  }
  const obj = value as Record<string, unknown>;
  if (obj.jsonrpc !== undefined && obj.jsonrpc !== "2.0") {
    return { kind: "error", error: { code: RpcErrorCode.InvalidRequest, message: "Invalid Request" } };
  }
  const method = obj.method;
  if (typeof method !== "string" || method.length === 0) {
    // 无 method 但带 id 与 result/error：响应帧（server 不会主动收到，识别后忽略）
    if ("id" in obj && ("result" in obj || "error" in obj)) {
      const id = obj.id;
      if (typeof id === "string" || typeof id === "number" || id === null) {
        const response: JsonRpcResponse = { jsonrpc: "2.0", id };
        if (obj.result !== undefined) response.result = obj.result;
        if (obj.error !== undefined) response.error = obj.error as JsonRpcError;
        return { kind: "response", response };
      }
    }
    return { kind: "error", error: { code: RpcErrorCode.InvalidRequest, message: "Invalid Request" } };
  }
  if ("id" in obj) {
    const id = obj.id;
    if (!(typeof id === "string" || typeof id === "number" || id === null)) {
      return { kind: "error", error: { code: RpcErrorCode.InvalidRequest, message: "Invalid Request" } };
    }
    return { kind: "request", request: { jsonrpc: "2.0", id, method, params: obj.params } };
  }
  return { kind: "notification", notification: { jsonrpc: "2.0", method, params: obj.params } };
}

export function encodeMessage(msg: JsonRpcMessage): string {
  return JSON.stringify(msg);
}

/** 解析失败的请求也要回一个错误响应（id: null）。 */
export function errorResponse(error: JsonRpcError): JsonRpcResponse {
  return { jsonrpc: "2.0", id: null, error };
}
