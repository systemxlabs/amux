import { HarnessUnavailableError, InvalidInputError, PromptTimeoutError, SessionBusyError, SessionClosedError, SessionNotFoundError } from "ahal";
import { describe, expect, it } from "vitest";
import { RpcErrorCode, ServerErrorCode } from "shared";
import { InvalidParamsError } from "./errors.js";
import { RpcServer } from "./rpc.js";

describe("RpcServer 分发核心", () => {
  it("请求 → 匹配 id 的响应（结果透传）", async () => {
    const rpc = new RpcServer();
    rpc.register("echo", async (p) => p);
    const res = await rpc.handle({ jsonrpc: "2.0", id: 42, method: "echo", params: { a: 1 } });
    expect(res).toEqual({ jsonrpc: "2.0", id: 42, result: { a: 1 } });
  });

  it("处理器返回 undefined → result 为 null", async () => {
    const rpc = new RpcServer();
    rpc.register("noop", async () => undefined);
    const res = await rpc.handle({ jsonrpc: "2.0", id: "x", method: "noop" });
    expect(res?.result).toBeNull();
  });

  it("通知 → 无响应（返回 null），处理器被调用", async () => {
    const rpc = new RpcServer();
    let called = 0;
    rpc.register("n", async () => {
      called++;
    });
    const res = await rpc.handle({ jsonrpc: "2.0", method: "n", params: {} });
    expect(res).toBeNull();
    expect(called).toBe(1);
  });

  it("未知方法 → -32601（请求）", async () => {
    const rpc = new RpcServer();
    const res = await rpc.handle({ jsonrpc: "2.0", id: 1, method: "nope" });
    expect(res?.error?.code).toBe(RpcErrorCode.MethodNotFound);
    expect(res?.id).toBe(1);
  });

  it("处理器抛 InvalidParamsError → -32602", async () => {
    const rpc = new RpcServer();
    rpc.register("m", async () => {
      throw new InvalidParamsError("缺参数");
    });
    const res = await rpc.handle({ jsonrpc: "2.0", id: "x", method: "m" });
    expect(res?.error?.code).toBe(RpcErrorCode.InvalidParams);
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
      expect(res?.error?.code).toBe(code);
      expect(res?.error?.message).toBe("x");
    }
  });

  it("未捕获 Error → -32603（内部错误）", async () => {
    const rpc = new RpcServer();
    rpc.register("m", async () => {
      throw new Error("boom");
    });
    const res = await rpc.handle({ jsonrpc: "2.0", id: 1, method: "m" });
    expect(res?.error?.code).toBe(RpcErrorCode.InternalError);
  });

  it("响应帧 → null（server 忽略）", async () => {
    const rpc = new RpcServer();
    const res = await rpc.handle({ jsonrpc: "2.0", id: 1, result: 5 });
    expect(res).toBeNull();
  });
});
