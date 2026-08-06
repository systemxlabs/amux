import { describe, expect, it } from "vitest";
import { RpcErrorCode } from "shared";
import { encodeMessage, parseJsonRpc } from "./jsonrpc.js";

describe("parseJsonRpc", () => {
  it("解析请求帧", () => {
    const f = parseJsonRpc('{"jsonrpc":"2.0","id":1,"method":"get_info","params":{}}');
    expect(f.kind).toBe("request");
    if (f.kind === "request") {
      expect(f.request.id).toBe(1);
      expect(f.request.method).toBe("get_info");
      expect(f.request.params).toEqual({});
    }
  });

  it("解析通知帧（无 id）", () => {
    const f = parseJsonRpc('{"jsonrpc":"2.0","method":"event","params":{}}');
    expect(f.kind).toBe("notification");
  });

  it("解析响应帧（server 识别后忽略）", () => {
    const f = parseJsonRpc('{"jsonrpc":"2.0","id":1,"result":5}');
    expect(f.kind).toBe("response");
    if (f.kind === "response") {
      expect(f.response.id).toBe(1);
      expect(f.response.result).toBe(5);
    }
  });

  it("非法 JSON → ParseError (-32700)", () => {
    const f = parseJsonRpc("{not json");
    expect(f.kind).toBe("error");
    if (f.kind === "error") expect(f.error.code).toBe(RpcErrorCode.ParseError);
  });

  it("非对象 → InvalidRequest (-32600)", () => {
    const f = parseJsonRpc("[1,2,3]");
    expect(f.kind).toBe("error");
    if (f.kind === "error") expect(f.error.code).toBe(RpcErrorCode.InvalidRequest);
  });

  it("缺 method → InvalidRequest", () => {
    const f = parseJsonRpc('{"jsonrpc":"2.0","id":1}');
    expect(f.kind).toBe("error");
  });

  it("jsonrpc 版本不是 2.0 → InvalidRequest", () => {
    const f = parseJsonRpc('{"jsonrpc":"1.0","method":"x","id":1}');
    expect(f.kind).toBe("error");
    if (f.kind === "error") expect(f.error.code).toBe(RpcErrorCode.InvalidRequest);
  });

  it("请求 id 类型非法 → InvalidRequest", () => {
    const f = parseJsonRpc('{"jsonrpc":"2.0","id":{"a":1},"method":"x"}');
    expect(f.kind).toBe("error");
  });

  it("encode 往返一致", () => {
    const msg = { jsonrpc: "2.0" as const, id: 7, method: "ping" };
    expect(JSON.parse(encodeMessage(msg))).toEqual(msg);
  });
});
