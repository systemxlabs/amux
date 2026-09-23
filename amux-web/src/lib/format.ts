// 展示格式化：时间、消息文本、活动摘要（docs/PRD.md「会话交互视图」「会话活动」）。

import type { Activity, ContentBlock } from "./types";

function pad(value: number, width = 2): string {
  return String(value).padStart(width, "0");
}

/** 本地时间 `YYYY-MM-DD HH:MM:SS`（消息气泡与活动条目展示用）。 */
export function formatTime(ms: number): string {
  const date = new Date(ms);
  if (Number.isNaN(date.getTime())) return "";
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
    `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
  );
}

/** 消息内容块的纯文本（气泡展示；非文本块退化为类型标签）。 */
export function blocksText(blocks: readonly ContentBlock[]): string {
  return blocks
    .map((block) => {
      switch (block.type) {
        case "text":
          return block.text;
        case "resource":
          if (block.text) return block.text;
          if (block.blob) return block.uri ? `[图片 ${block.uri}]` : "[图片]";
          return block.uri ? `[资源 ${block.uri}]` : "[资源]";
        case "resource_link":
          return `[引用 ${block.title ?? block.name}]`;
      }
    })
    .join("");
}

/** 一行展示的文本：压缩空白并把换行折叠为可见分隔。 */
export function oneLine(text: string): string {
  return text.split(/\s+/).filter((part) => part !== "").join(" ");
}

/** 活动一行摘要（活动条目与「实时活动」折叠态展示）。 */
export function activitySummary(activity: Activity): string {
  switch (activity.kind) {
    case "thinking":
      return oneLine(activity.thinking);
    case "tool_call":
      return oneLine(activityContent(activity));
    case "error":
      return oneLine(activity.error);
  }
}

/** 实时活动条文案：`<活动类型> <活动内容>`（docs/PRD.md「会话交互视图」）。 */
export function activityBarText(activity: Activity): string {
  return `${activityKindLabel(activity)} ${activitySummary(activity)}`;
}

/** 活动详情（展开态展示）。 */
export function activityDetail(activity: Activity): string {
  switch (activity.kind) {
    case "thinking":
      return activity.thinking;
    case "tool_call":
      return activityContent(activity);
    case "error":
      return activity.error;
  }
}

/** 工具调用内容：`tool_name(tool_title)` + 可选参数。 */
function activityContent(activity: Extract<Activity, { kind: "tool_call" }>): string {
  let content = activity.tool_name;
  if (activity.title) content += `(${activity.title})`;
  if (activity.parameters) content += `\n${activity.parameters}`;
  return content;
}

/** 活动类型标签。 */
export function activityKindLabel(activity: Activity): string {
  switch (activity.kind) {
    case "thinking":
      return "思考";
    case "tool_call":
      return "工具调用";
    case "error":
      return "错误";
  }
}

/** 会话状态标签。 */
export function stateLabel(state: "idle" | "busy"): string {
  return state === "busy" ? "工作中" : "空闲";
}

/** 截断到 `max` 字符（超出追加省略号）。 */
export function truncate(text: string, max: number): string {
  const chars = [...text];
  return chars.length <= max ? text : `${chars.slice(0, max).join("")}…`;
}

/** 上下文用量展示。 */
export function formatContext(contextSize: number, windowSize: number): string {
  if (windowSize === 0) return "—";
  return `${contextSize} / ${windowSize}`;
}
