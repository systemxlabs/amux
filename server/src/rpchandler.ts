/**
 * 方法处理器：把 shared 协议方法面接到 SessionManager / GitRunner / HarnessRegistry。
 * 参数校验在这里做（非法参数 → -32602；输入内容非法 → InvalidInputError → -32005）。
 * handler 返回 RpcOutcome（result + 可选的 afterSend），补发缺口在响应发送后执行。
 */

import { InvalidInputError, SessionBusyError, type ContentBlock, type Input } from "ahal";
import { Methods, type HarnessName } from "shared";
import { InvalidParamsError } from "./errors.js";
import { GitRunner } from "./git.js";
import { HarnessRegistry } from "./harness.js";
import type { RpcHandler, RpcOutcome } from "./rpc.js";
import { SessionManager } from "./sessions.js";

export interface HandlerDeps {
  manager: SessionManager;
  git: GitRunner;
  harnesses: HarnessRegistry;
  serverVersion: string;
}

// ---- 参数校验 ----

function expectObject(params: unknown): Record<string, unknown> {
  if (typeof params !== "object" || params === null || Array.isArray(params)) {
    throw new InvalidParamsError("参数必须是对象");
  }
  return params as Record<string, unknown>;
}

function str(p: Record<string, unknown>, key: string): string {
  const v = p[key];
  if (typeof v !== "string" || v.length === 0) throw new InvalidParamsError(`参数 ${key} 必须是非空字符串`);
  return v;
}

function optStr(p: Record<string, unknown>, key: string): string | undefined {
  const v = p[key];
  if (v === undefined) return undefined;
  if (typeof v !== "string") throw new InvalidParamsError(`参数 ${key} 必须是字符串`);
  return v;
}

/** 校验并规范化 prompt 输入（ahal Input = ContentBlock[]）。 */
export function validateInput(v: unknown): Input {
  if (!Array.isArray(v) || v.length === 0) throw new InvalidInputError("prompt 输入必须是非空内容块数组");
  const out: ContentBlock[] = [];
  for (const item of v) {
    if (typeof item !== "object" || item === null) throw new InvalidInputError("内容块必须是对象");
    const b = item as Record<string, unknown>;
    if (b.type === "text") {
      if (typeof b.text !== "string") throw new InvalidInputError("text 块缺少文本");
      out.push({ type: "text", text: b.text });
    } else if (b.type === "resource") {
      if (typeof b.mimeType !== "string") throw new InvalidInputError("resource 块缺少 mimeType");
      const uri = typeof b.uri === "string" ? { uri: b.uri } : {};
      if (typeof b.text === "string") out.push({ type: "resource", mimeType: b.mimeType, text: b.text, ...uri });
      else if (typeof b.blob === "string") out.push({ type: "resource", mimeType: b.mimeType, blob: b.blob, ...uri });
      else throw new InvalidInputError("resource 块缺少 text 或 blob");
    } else if (b.type === "resource_link") {
      if (typeof b.uri !== "string" || b.uri.length === 0) throw new InvalidInputError("resource_link 块缺少 uri");
      if (typeof b.name !== "string" || b.name.length === 0) throw new InvalidInputError("resource_link 块缺少 name");
      const extra: Record<string, unknown> = {};
      if (typeof b.mimeType === "string") extra.mimeType = b.mimeType;
      if (typeof b.title === "string") extra.title = b.title;
      if (typeof b.description === "string") extra.description = b.description;
      if (typeof b.size === "number") extra.size = b.size;
      out.push({ type: "resource_link", uri: b.uri, name: b.name, ...extra });
    } else {
      throw new InvalidInputError(`未知内容块类型: ${String(b.type)}`);
    }
  }
  return out;
}

// ---- 方法表 ----

export function buildMethodHandlers(deps: HandlerDeps): Record<string, RpcHandler> {
  const ok = (result: unknown, afterSend?: () => void): RpcOutcome => ({ result, afterSend });
  return {
    [Methods.GetInfo]: async () =>
      ok({
        info: { serverVersion: deps.serverVersion, harnesses: deps.harnesses.info() },
      }),

    [Methods.ListSessions]: async () => ok({ sessions: deps.manager.list() }),

    [Methods.CreateSession]: async (params) => {
      const p = expectObject(params);
      const harness = str(p, "harness");
      if (!deps.harnesses.has(harness)) throw new InvalidParamsError(`未知 harness: ${harness}`);
      const session = await deps.manager.create({ harness: harness as HarnessName, cwd: str(p, "cwd"), model: optStr(p, "model") });
      return ok({ session });
    },

    [Methods.ResumeSession]: async (params) => {
      const p = expectObject(params);
      const session = await deps.manager.resume(str(p, "sessionId"));
      return ok({ session });
    },

    [Methods.CloseSession]: async (params) => {
      const p = expectObject(params);
      await deps.manager.close(str(p, "sessionId"));
      return ok(null);
    },

    [Methods.DeleteSession]: async (params) => {
      const p = expectObject(params);
      await deps.manager.delete(str(p, "sessionId"));
      return ok(null);
    },

    [Methods.Prompt]: async (params) => {
      const p = expectObject(params);
      const sessionId = str(p, "sessionId");
      const input = validateInput(p.input);
      await deps.manager.prompt(sessionId, input);
      return ok(null);
    },

    [Methods.Cancel]: async (params) => {
      const p = expectObject(params);
      await deps.manager.cancel(str(p, "sessionId"));
      return ok(null);
    },

    /**
     * get_history：返回持久化历史（按 jsonl 追加顺序），并在同一同步块内
     * 标记该连接在该会话上的补齐位置（内部序号）——历史快照与位置原子对齐，
     * 之后到达的实时项由广播层按连接补齐路由；暂存缺口在响应发送后补发。
     */
    [Methods.GetHistory]: async (params, ctx) => {
      const p = expectObject(params);
      const sessionId = str(p, "sessionId");
      const items = deps.manager.historyFor(sessionId);
      const pos = deps.manager.currentOrder(sessionId);
      const pendings = ctx?.catchup.mark(sessionId, pos) ?? [];
      return ok(
        { items },
        () => {
          for (const it of pendings) ctx?.send(it.method, it.params);
        },
      );
    },

    [Methods.GitStatus]: async (params) => {
      const p = expectObject(params);
      return ok(await deps.git.status(str(p, "cwd")));
    },

    [Methods.GitDiff]: async (params) => {
      const p = expectObject(params);
      const diff = await deps.git.diff(str(p, "cwd"), optStr(p, "path"));
      return ok({ diff });
    },

    [Methods.GitPush]: async (params) => {
      const p = expectObject(params);
      return ok(await deps.git.push(str(p, "cwd")));
    },

    [Methods.GitRevert]: async (params) => {
      const p = expectObject(params);
      const cwd = str(p, "cwd");
      const path = optStr(p, "path");
      const patch = optStr(p, "patch");
      if (patch !== undefined && path === undefined) throw new InvalidParamsError("hunk 级撤销必须同时指定 path");
      // undo 语义：等会话工作区间结束（docs/DESIGN.md §4）
      if (deps.manager.anyBusyInCwd(cwd)) {
        throw new SessionBusyError("会话工作中，撤销需等工作区间结束");
      }
      return ok(await deps.git.revert(cwd, { path, patch }));
    },
  };
}
