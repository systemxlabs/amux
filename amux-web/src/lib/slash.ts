// 斜杠命令前缀匹配（docs/PRD.md「会话交互视图」：输入 `/` 时弹出上拉框）。

import type { SlashCommand } from "./types";

/**
 * 触发上拉框的输入前缀；不触发时返回 null。
 *
 * 仅在输入以 `/` 开头且尚未输入空格（即仍在校对命令名）时触发；
 * `/` 单独输入时前缀为空串，匹配全部命令。
 */
export function slashPrefix(input: string): string | null {
  if (!input.startsWith("/")) return null;
  const rest = input.slice(1);
  if (/\s/.test(rest)) return null;
  return rest;
}

/** 前缀匹配的命令列表（前缀为空时返回全部）。 */
export function matchSlashCommands(
  commands: readonly SlashCommand[],
  input: string,
): SlashCommand[] {
  const prefix = slashPrefix(input);
  if (prefix === null) return [];
  return commands.filter((command) => command.name.startsWith(prefix));
}
