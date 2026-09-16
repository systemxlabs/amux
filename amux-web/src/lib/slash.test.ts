// 斜杠命令上拉框的触发与匹配（PRD「会话交互视图」）。

import { describe, expect, it } from "vitest";

import { matchSlashCommands, slashPrefix } from "./slash";
import type { SlashCommand } from "./types";

const commands: SlashCommand[] = [
  { name: "goal", description: "设定目标" },
  { name: "go", description: "go" },
  { name: "clear", description: "清空" },
];

describe("slashPrefix", () => {
  it("以 / 开头且未输入空格时返回命令名前缀", () => {
    expect(slashPrefix("/")).toBe("");
    expect(slashPrefix("/go")).toBe("go");
  });

  it("普通输入与已带参数的命令不触发上拉框", () => {
    expect(slashPrefix("你好")).toBeNull();
    expect(slashPrefix("/goal 完成登录")).toBeNull();
  });
});

describe("matchSlashCommands", () => {
  it("前缀为空时列出全部命令", () => {
    expect(matchSlashCommands(commands, "/").map((command) => command.name)).toEqual([
      "goal",
      "go",
      "clear",
    ]);
  });

  it("按前缀匹配命令名", () => {
    expect(matchSlashCommands(commands, "/go").map((command) => command.name)).toEqual([
      "goal",
      "go",
    ]);
  });

  it("不触发上拉框时返回空列表", () => {
    expect(matchSlashCommands(commands, "hello")).toEqual([]);
  });
});
