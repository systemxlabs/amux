// 群聊 @ 提及自动补全（docs/PRD.md「会话交互视图」）。

import type { GroupMember } from "./types";

export type MentionPrefix = {
  start: number;
  prefix: string;
};

export type MentionCandidate = {
  name: string;
  description: string;
};

const ALL_MENTION: MentionCandidate = {
  name: "all",
  description: "所有 agent 成员",
};

/**
 * 当前正在输入的提及前缀。
 *
 * 以最后一个 `@` 为起点；其后的内容出现空白后视为提及已结束。
 */
export function mentionPrefix(input: string): MentionPrefix | null {
  const start = input.lastIndexOf("@");
  if (start === -1) return null;
  const prefix = input.slice(start + 1);
  if (/\s/u.test(prefix)) return null;
  return { start, prefix };
}

/** 前缀匹配的特殊提及与 agent 成员（大小写敏感）。 */
export function matchMentions(
  members: readonly GroupMember[],
  input: string,
): MentionCandidate[] {
  const mention = mentionPrefix(input);
  if (mention === null) return [];
  return [ALL_MENTION, ...members].filter((candidate) =>
    candidate.name.startsWith(mention.prefix),
  );
}

/** 用选中的提及替换当前前缀，并补一个空格便于继续输入。 */
export function completeMention(input: string, name: string): string {
  const mention = mentionPrefix(input);
  if (mention === null) return input;
  return `${input.slice(0, mention.start)}@${name} `;
}
