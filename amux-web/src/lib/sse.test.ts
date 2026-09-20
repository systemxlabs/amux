import { describe, expect, it } from "vitest";

import { createSseDecoder } from "./sse";

describe("createSseDecoder", () => {
  it("跨网络分块解析事件", () => {
    const decoder = createSseDecoder();
    expect(decoder.push("event: output\ndata: {\"data\":\"a")).toEqual([]);
    expect(decoder.push("Gk=\"}\n\n")).toEqual(['{"data":"aGk="}']);
  });

  it("支持 CRLF 与多行 data", () => {
    const decoder = createSseDecoder();
    expect(decoder.push("data: first\r\ndata: second\r\n\r\n")).toEqual(["first\nsecond"]);
  });

  it("忽略 keep-alive 注释", () => {
    const decoder = createSseDecoder();
    expect(decoder.push(": keep-alive\n\n")).toEqual([]);
  });
});
