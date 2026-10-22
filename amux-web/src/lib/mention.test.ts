import { describe, expect, it } from "vitest";

import { completeMention, matchMentions, mentionPrefix } from "./mention";
import type { GroupMember } from "./types";

const members: GroupMember[] = [
  {
    id: "m1",
    name: "coder",
    description: "负责实现",
    machine: "pc",
    agent: "codex",
    sessionId: "s1",
    pinned: false,
    createdAt: 1,
    updatedAt: 1,
  },
  {
    id: "m2",
    name: "reviewer",
    description: "负责审查",
    machine: "pc",
    agent: "claude",
    sessionId: "s2",
    pinned: false,
    createdAt: 2,
    updatedAt: 2,
  },
];

describe("mentionPrefix", () => {
  it("定位当前尚未结束的提及", () => {
    expect(mentionPrefix("@")).toEqual({ start: 0, prefix: "" });
    expect(mentionPrefix("请 @co")).toEqual({ start: 2, prefix: "co" });
  });

  it("提及后输入空白或普通文本时不触发", () => {
    expect(mentionPrefix("你好")).toBeNull();
    expect(mentionPrefix("@coder 开始")).toBeNull();
  });
});

describe("matchMentions", () => {
  it("列出特殊提及和匹配前缀的成员", () => {
    expect(matchMentions(members, "@").map((item) => item.name)).toEqual([
      "all",
      "coder",
      "reviewer",
    ]);
    expect(matchMentions(members, "@c").map((item) => item.name)).toEqual(["coder"]);
  });

  it("按大小写敏感的前缀过滤", () => {
    expect(matchMentions(members, "@C")).toEqual([]);
  });
});

describe("completeMention", () => {
  it("替换当前前缀并补尾随空格", () => {
    expect(completeMention("请 @co", "coder")).toBe("请 @coder ");
  });
});
