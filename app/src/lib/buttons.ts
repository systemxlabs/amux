/**
 * 快捷按钮注册表（客户端本地配置，无专用协议）。
 * 映射语义依据 docs/DESIGN.md §4：
 * - prompt 型（commit & push / submit PR 等需要编写内容）：经 prompt 由 agent 执行
 * - delete：客户端直接发起对应会话操作
 * 注意：会话级操作（New Session / Kill / Delete）在侧边栏菜单，按钮栏只保留
 * 「与当前会话工作区相关的操作」。
 */

import type { SessionState } from "ahal";

export type ButtonKind = "prompt" | "git-push" | "git-revert" | "new-session" | "kill-session" | "delete-session";

export interface ActionButton {
  id: string;
  label: string;
  kind: ButtonKind;
  /** prompt 型按钮发送给 agent 的指令模板 */
  promptTemplate?: string;
  enabled: boolean;
}

export const DEFAULT_BUTTONS: ActionButton[] = [
  {
    id: "commit-push",
    label: "Commit & Push",
    kind: "prompt",
    promptTemplate: "提交并推送当前工作区的更改：为改动写一条简洁的 commit message，commit 后 push。",
    enabled: true,
  },
  {
    id: "submit-pr",
    label: "Submit PR",
    kind: "prompt",
    promptTemplate: "提交一个 Pull Request：stage → commit → push → 创建 PR。",
    enabled: true,
  },
];

/**
 * 按钮可用性：
 * - prompt 型需要会话可用（未关闭、未中断）
 * （会话级操作在侧边栏菜单，不属于按钮栏）
 */
export function buttonDisabled(button: ActionButton, _state: SessionState, closed: boolean, interrupted: boolean): boolean {
  if (!button.enabled) return true;
  switch (button.kind) {
    case "git-push":
    case "prompt":
      return closed || interrupted;
    default:
      return false;
  }
}
