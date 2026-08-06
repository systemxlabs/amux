/**
 * JSON-RPC 2.0 信封类型与错误码（协议层，与业务方法无关）。
 * 语义依据：docs/DESIGN.md §3「传输与消息」。
 */

export type JsonRpcId = string | number | null;

export interface JsonRpcRequest {
  jsonrpc: "2.0";
  id: JsonRpcId;
  method: string;
  params?: unknown;
}

export interface JsonRpcNotification {
  jsonrpc: "2.0";
  method: string;
  params?: unknown;
}

export interface JsonRpcError {
  code: number;
  message: string;
  data?: unknown;
}

export interface JsonRpcResponse {
  jsonrpc: "2.0";
  id: JsonRpcId;
  result?: unknown;
  error?: JsonRpcError;
}

export type JsonRpcMessage = JsonRpcRequest | JsonRpcNotification | JsonRpcResponse;

/** JSON-RPC 2.0 标准错误码 */
export const RpcErrorCode = {
  ParseError: -32700,
  InvalidRequest: -32600,
  MethodNotFound: -32601,
  InvalidParams: -32602,
  InternalError: -32603,
} as const;

/**
 * 服务器错误区间 -32000..-32099：AHAL 错误与 server 业务错误的映射。
 * 与 ahal 错误类一一对应（ahal: AhalError / SessionNotFoundError / HarnessUnavailableError /
 * SessionBusyError / SessionClosedError / InvalidInputError / PromptTimeoutError）。
 */
export const ServerErrorCode = {
  /** AhalError 基类及其他未归类服务器错误 */
  AhalError: -32000,
  /** SessionNotFoundError */
  SessionNotFound: -32001,
  /** HarnessUnavailableError */
  HarnessUnavailable: -32002,
  /** SessionBusyError；git revert 在会话忙时被拒绝也复用此码 */
  SessionBusy: -32003,
  /** SessionClosedError */
  SessionClosed: -32004,
  /** InvalidInputError */
  InvalidInput: -32005,
  /** PromptTimeoutError */
  PromptTimeout: -32006,
} as const;
