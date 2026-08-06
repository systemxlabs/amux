/**
 * 快捷按钮注册表（客户端本地配置，无专用协议）。
 * 映射语义依据 docs/DESIGN.md §4：
 * - prompt 型（commit / submit PR 等需要编写内容）：经 prompt 由 agent 执行
 * - git 型（push / undo）：server 直连 git；undo/revert 需等工作区间结束
 * - 会话操作（new / kill）：客户端直接发起对应会话操作
 */

import type { SessionState } from "ahal";

export type ButtonKind = "prompt" | "git-push" | "git-revert" | "new-session" | "kill-session";

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
    id: "commit",
    label: "Commit",
    kind: "prompt",
    promptTemplate: "提交当前工作区的更改：为改动写一条简洁的 commit message 并执行 commit。",
    enabled: true,
  },
  { id: "push", label: "Push", kind: "git-push", enabled: true },
  {
    id: "submit-pr",
    label: "Submit PR",
    kind: "prompt",
    promptTemplate: "提交一个 Pull Request：stage → commit → push → 创建 PR。",
    enabled: true,
  },
  { id: "undo", label: "Undo", kind: "git-revert", enabled: true },
  { id: "new-session", label: "New Session", kind: "new-session", enabled: true },
  { id: "kill", label: "Kill Session", kind: "kill-session", enabled: true },
];

/**
 * 按钮可用性：
 * - undo/revert 需等工作区间结束（state 非 idle、已关闭、已中断时禁用）
 * - prompt 型需要会话可用（未关闭、未中断）
 * - push 需要会话可用
 * - kill / new-session 始终可用
 */
export function buttonDisabled(button: ActionButton, state: SessionState, closed: boolean, interrupted: boolean): boolean {
  if (!button.enabled) return true;
  switch (button.kind) {
    case "git-revert":
      return state !== "idle" || closed || interrupted;
    case "git-push":
    case "prompt":
      return closed || interrupted;
    default:
      return false;
  }
}
