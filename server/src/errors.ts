/**
 * 服务器侧错误：RpcError（分发层）与 AHAL 错误 → JSON-RPC 错误码映射。
 */

import {
  AhalError,
  HarnessUnavailableError,
  InvalidInputError,
  PromptTimeoutError,
  SessionBusyError,
  SessionClosedError,
  SessionNotFoundError,
} from "ahal";
import { RpcErrorCode, ServerErrorCode, type JsonRpcError } from "shared";

/** 分发层错误（携带 JSON-RPC 错误码；非 AHAL 错误）。 */
export class RpcError extends Error {
  constructor(
    public readonly code: number,
    message: string,
  ) {
    super(message);
    this.name = "RpcError";
  }
}

export class InvalidParamsError extends RpcError {
  constructor(message: string) {
    super(RpcErrorCode.InvalidParams, message);
    this.name = "InvalidParamsError";
  }
}

/** 把任意抛出的错误映射为 JSON-RPC 错误。顺序敏感：具体错误类先于基类。 */
export function toRpcError(err: unknown): JsonRpcError {
  if (err instanceof SessionNotFoundError) return { code: ServerErrorCode.SessionNotFound, message: err.message };
  if (err instanceof HarnessUnavailableError) return { code: ServerErrorCode.HarnessUnavailable, message: err.message };
  if (err instanceof SessionBusyError) return { code: ServerErrorCode.SessionBusy, message: err.message };
  if (err instanceof SessionClosedError) return { code: ServerErrorCode.SessionClosed, message: err.message };
  if (err instanceof InvalidInputError) return { code: ServerErrorCode.InvalidInput, message: err.message };
  if (err instanceof PromptTimeoutError) return { code: ServerErrorCode.PromptTimeout, message: err.message };
  if (err instanceof AhalError) return { code: ServerErrorCode.AhalError, message: err.message };
  if (err instanceof RpcError) return { code: err.code, message: err.message };
  if (err instanceof Error) return { code: RpcErrorCode.InternalError, message: err.message };
  return { code: RpcErrorCode.InternalError, message: String(err) };
}
