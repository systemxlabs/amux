import { describe, expect, it } from "vitest";
import { contentToText, eventsToView } from "./viewModel.js";

describe("eventsToView（事件 → 对话视图）", () => {
  it("chunk 累积 + 最终事件覆盖为完整气泡", () => {
    const view = eventsToView([
      { event: { kind: "agent_thought_chunk", messageId: "t1", content: { type: "text", text: "思" } }, timestamp: 1 },
      { event: { kind: "agent_thought_chunk", messageId: "t1", content: { type: "text", text: "考中" } }, timestamp: 2 },
      { event: { kind: "agent_thought", messageId: "t1", content: [{ type: "text", text: "思考完毕" }] }, timestamp: 3 },
      { event: { kind: "agent_message_chunk", messageId: "m1", content: { type: "text", text: "你" } }, timestamp: 4 },
      { event: { kind: "agent_message", messageId: "m1", content: [{ type: "text", text: "你好" }] }, timestamp: 5 },
    ]);
    expect(view.bubbles).toHaveLength(2);
    expect(view.bubbles[0].kind).toBe("thought");
    expect(view.bubbles[0].content).toEqual([{ type: "text", text: "思考完毕" }]); // 最终覆盖 chunk
    expect(view.bubbles[0].final).toBe(true);
    expect(view.bubbles[1].kind).toBe("message");
    expect(view.bubbles[1].content).toEqual([{ type: "text", text: "你好" }]);
    expect(view.lastEventAt).toBe(5);
  });

  it("工具调用：状态与内容更新", () => {
    const view = eventsToView([
      { event: { kind: "tool_call_update", toolCallId: "tc1", toolName: "shell", status: "in_progress", title: "运行命令" }, timestamp: 1 },
      { event: { kind: "tool_call_content_chunk", toolCallId: "tc1", content: { type: "text", text: "输出片段" } }, timestamp: 2 },
      { event: { kind: "tool_call_update", toolCallId: "tc1", status: "completed" }, timestamp: 3 },
    ]);
    expect(view.tools).toHaveLength(1);
    expect(view.tools[0]).toMatchObject({ key: "tc1", name: "shell", status: "completed", title: "运行命令" });
    expect(view.tools[0].content).toEqual([{ type: "text", text: "输出片段" }]);
  });

  it("状态、用量、错误", () => {
    const view = eventsToView([
      { event: { kind: "state_changed", state: "thinking" }, timestamp: 1 },
      { event: { kind: "usage_update", context: 100, contextWindow: 200000 }, timestamp: 2 },
      { event: { kind: "state_changed", state: "idle", reason: "end_turn" }, timestamp: 3 },
      { event: { kind: "error", message: "boom" }, timestamp: 4 },
    ]);
    expect(view.state).toBe("idle");
    expect(view.usage).toEqual({ context: 100, contextWindow: 200000 });
    expect(view.errors).toEqual(["boom"]);
  });
});

describe("contentToText", () => {
  it("文本/资源/引用混合", () => {
    expect(
      contentToText([
        { type: "text", text: "hi" },
        { type: "resource", mimeType: "image/png", blob: "x" },
        { type: "resource_link", uri: "file:///a.ts", name: "a.ts" },
      ]),
    ).toBe("hi\n[image/png 资源]\n[引用 file:///a.ts]");
  });
});
