// 消息与活动展示格式化（PRD「会话交互视图」「会话活动」）。

import { describe, expect, it } from "vitest";

import {
  activityBarText,
  activityDetail,
  activitySummary,
  blocksText,
  formatTime,
  oneLine,
  truncate,
} from "./format";

describe("formatTime", () => {
  it("按本地时间输出到秒", () => {
    const ms = new Date(2026, 0, 2, 3, 4, 5).getTime();
    expect(formatTime(ms)).toBe("2026-01-02 03:04:05");
  });

  it("非法时间戳返回空串", () => {
    expect(formatTime(Number.NaN)).toBe("");
  });
});

describe("blocksText", () => {
  it("拼接文本块并对非文本块给出标签", () => {
    expect(
      blocksText([
        { type: "text", text: "看这个 " },
        { type: "resource_link", uri: "file:///a.rs", name: "a.rs", title: "a.rs" },
        { type: "resource", mimeType: "text/plain", uri: "file:///b.rs" },
      ]),
    ).toBe("看这个 [引用 a.rs][资源 file:///b.rs]");
  });

  it("资源块的文本内容与图片 blob 不再退化为通用标签", () => {
    expect(
      blocksText([
        { type: "resource", mimeType: "text/plain", uri: "a.txt", text: "hello\nworld" },
        { type: "resource", mimeType: "image/png", uri: "pic.png", blob: "AAAA" },
      ]),
    ).toBe("hello\nworld[图片 pic.png]");
  });
});

describe("activitySummary", () => {
  it("工具调用展示工具名和可选标题，换行折叠为一行", () => {
    expect(
      activitySummary({
        kind: "tool_call",
        id: "a1",
        timestamp: 1,
        tool_call_id: "tc1",
        tool_name: "read_file",
        title: "读取\nsrc/lib.rs",
      }),
    ).toBe("read_file(读取 src/lib.rs)");
  });

  it("无标题时退回工具名；思考与错误取各自内容", () => {
    expect(
      activitySummary({
        kind: "tool_call",
        id: "a1",
        timestamp: 1,
        tool_call_id: "tc1",
        tool_name: "read_file",
      }),
    ).toBe("read_file");
    expect(activitySummary({ kind: "thinking", id: "a2", timestamp: 1, thinking: "先看看" })).toBe(
      "先看看",
    );
    expect(activitySummary({ kind: "error", id: "a3", timestamp: 1, error: "调用失败" })).toBe(
      "调用失败",
    );
  });
});

describe("activityBarText", () => {
  it("实时活动条为「活动类型 活动内容」", () => {
    expect(
      activityBarText({
        kind: "tool_call",
        id: "a1",
        timestamp: 1,
        tool_call_id: "tc1",
        tool_name: "read_file",
        title: "读取 src/lib.rs",
      }),
    ).toBe("工具调用 read_file(读取 src/lib.rs)");
    expect(activityBarText({ kind: "thinking", id: "a2", timestamp: 1, thinking: "先看看" })).toBe(
      "思考 先看看",
    );
    expect(activityBarText({ kind: "error", id: "a3", timestamp: 1, error: "调用失败" })).toBe(
      "错误 调用失败",
    );
  });
});

describe("activityDetail", () => {
  it("工具调用参数另起一行，无标题和参数时只展示工具名", () => {
    expect(
      activityDetail({
        kind: "tool_call",
        id: "a1",
        timestamp: 1,
        tool_call_id: "tc1",
        tool_name: "prompt_session",
        parameters: '{"session":"s1"}',
      }),
    ).toBe('prompt_session\n{"session":"s1"}');
    expect(
      activityDetail({
        kind: "tool_call",
        id: "a2",
        timestamp: 2,
        tool_call_id: "tc2",
        tool_name: "list_agents",
      }),
    ).toBe("list_agents");
  });
});

describe("oneLine 与 truncate", () => {
  it("压缩空白", () => {
    expect(oneLine(" a\n\n b  c ")).toBe("a b c");
  });

  it("按字符截断（中文按字符计）", () => {
    expect(truncate("中文内容", 3)).toBe("中文内…");
    expect(truncate("短", 3)).toBe("短");
  });
});
