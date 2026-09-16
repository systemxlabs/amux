// 终端游标增量与 base64 输出解码（DESIGN「终端视图」）。

import { describe, expect, it } from "vitest";

import {
  appendTerminalOutput,
  decodeBase64,
  encodeBase64,
  newTerminalStream,
} from "./terminal";

describe("decodeBase64", () => {
  it("解码服务端 base64 输出为原始字节", () => {
    expect(new TextDecoder().decode(decodeBase64("aGk="))).toBe("hi");
  });

  it("空串解码为空字节", () => {
    expect(decodeBase64("")).toHaveLength(0);
  });

  it("与编码互为逆运算（含非 ASCII 字节）", () => {
    const bytes = new Uint8Array([0x1b, 0x5b, 0x33, 0x31, 0x6d, 0xe4, 0xb8, 0xad]);
    expect(decodeBase64(encodeBase64(bytes))).toEqual(bytes);
  });
});

describe("appendTerminalOutput", () => {
  it("首次拉取从头开始，随后携带服务端返回的游标", () => {
    const stream = newTerminalStream();
    expect(stream.cursor).toBeNull();
    const first = appendTerminalOutput({ data: "YQ==", nextCursor: 1 });
    expect(first.reset).toBe(false);
    expect(new TextDecoder().decode(first.bytes)).toBe("a");
    expect(first.stream.cursor).toBe(1);
  });

  it("服务端丢弃旧输出（truncated）时要求重建缓冲", () => {
    const next = appendTerminalOutput({ data: "YmM=", nextCursor: 2, truncated: true });
    expect(next.reset).toBe(true);
    expect(new TextDecoder().decode(next.bytes)).toBe("bc");
    expect(next.stream.cursor).toBe(2);
  });
});
