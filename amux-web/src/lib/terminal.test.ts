// 终端 base64 输出编解码。

import { describe, expect, it } from "vitest";

import { decodeBase64, encodeBase64 } from "./terminal";

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
