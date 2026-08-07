import { HarnessUnavailableError, InvalidInputError, PromptTimeoutError, SessionBusyError, SessionClosedError, SessionNotFoundError } from "ahal";
import { describe, expect, it } from "vitest";
import { RpcErrorCode, ServerErrorCode } from "shared";
import { ConnectionCatchup } from "./catchup.js";
import { InvalidParamsError } from "./errors.js";
import { RpcServer } from "./rpc.js";

describe("RpcServer 分发核心", () => {
  it("请求 → 匹配 id 的响应（结果透传）", async () => {
    const rpc = new RpcServer();
    rpc.register("echo", async (p) => ({ result: p }));
    const res = await rpc.handle({ jsonrpc: "2.0", id: 42, method: "echo", params: { a: 1 } });
    expect(res?.response).toEqual({ jsonrpc: "2.0", id: 42, result: { a: 1 } });
    expect(res?.afterSend).toBeUndefined();
  });

  it("处理器返回 result null → 响应 result 为 null", async () => {
    const rpc = new RpcServer();
    rpc.register("noop", async () => ({ result: undefined }));
    const res = await rpc.handle({ jsonrpc: "2.0", id: "x", method: "noop" });
    expect(res?.response?.result).toBeNull();
  });

  it("afterSend：响应返回后执行（缺口补发时序钩子）", async () => {
    const rpc = new RpcServer();
    const order: string[] = [];
    rpc.register("with-after", async () => ({
      result: { items: [] },
      afterSend: () => order.push("afterSend"),
    }));
    const res = await rpc.handle({ jsonrpc: "2.0", id: 1, method: "with-after" });
    expect(res?.response?.result).toEqual({ items: [] });
    // 模拟调用方先发响应再执行 afterSend（index.ts 的时序）
    order.push("response");
    res?.afterSend?.();
    expect(order).toEqual(["response", "afterSend"]);
  });

  it("通知 → 无响应（response 为 null），处理器被调用", async () => {
    const rpc = new RpcServer();
    let called = 0;
    rpc.register("n", async () => {
      called++;
      return { result: null };
    });
    const res = await rpc.handle({ jsonrpc: "2.0", method: "n", params: {} });
    expect(res?.response).toBeNull();
    expect(called).toBe(1);
  });

  it("handler 携带连接上下文（RpcContext：catchup + send）", async () => {
    const rpc = new RpcServer();
    let seen: unknown;
    rpc.register("ctx", async (_p, ctx) => {
      seen = { hasCatchup: ctx?.catchup instanceof ConnectionCatchup, send: typeof ctx?.send };
      return { result: null };
    });
    const catchup = new ConnectionCatchup();
    await rpc.handle({ jsonrpc: "2.0", id: 1, method: "ctx" }, { catchup, send: () => {} });
    expect(seen).toEqual({ hasCatchup: true, send: "function" });
  });

  it("未知方法 → -32601（请求）", async () => {
    const rpc = new RpcServer();
    const res = await rpc.handle({ jsonrpc: "2.0", id: 1, method: "nope" });
    expect(res?.response?.error?.code).toBe(RpcErrorCode.MethodNotFound);
    expect(res?.response?.id).toBe(1);
  });

  it("处理器抛 InvalidParamsError → -32602", async () => {
    const rpc = new RpcServer();
    rpc.register("m", async () => {
      throw new InvalidParamsError("缺参数");
    });
    const res = await rpc.handle({ jsonrpc: "2.0", id: "x", method: "m" });
    expect(res?.response?.error?.code).toBe(RpcErrorCode.InvalidParams);
  });

  it("AHAL 错误 → 对应服务器错误码（-32000..-32099）", async () => {
    const cases: Array<[Error, number]> = [
      [new SessionNotFoundError("x"), ServerErrorCode.SessionNotFound],
      [new HarnessUnavailableError("x"), ServerErrorCode.HarnessUnavailable],
      [new SessionBusyError("x"), ServerErrorCode.SessionBusy],
      [new SessionClosedError("x"), ServerErrorCode.SessionClosed],
      [new InvalidInputError("x"), ServerErrorCode.InvalidInput],
      [new PromptTimeoutError("x"), ServerErrorCode.PromptTimeout],
    ];
    for (const [err, code] of cases) {
      const rpc = new RpcServer();
      rpc.register("m", async () => {
        throw err;
      });
      const res = await rpc.handle({ jsonrpc: "2.0", id: 1, method: "m" });
      expect(res?.response?.error?.code).toBe(code);
      expect(res?.response?.error?.message).toBe("x");
    }
  });

  it("未捕获 Error → -32603（内部错误）", async () => {
    const rpc = new RpcServer();
    rpc.register("m", async () => {
      throw new Error("boom");
    });
    const res = await rpc.handle({ jsonrpc: "2.0", id: 1, method: "m" });
    expect(res?.response?.error?.code).toBe(RpcErrorCode.InternalError);
  });

  it("响应帧 → response 为 null（server 忽略）", async () => {
    const rpc = new RpcServer();
    const res = await rpc.handle({ jsonrpc: "2.0", id: 1, result: 5 });
    expect(res?.response).toBeNull();
  });
});
