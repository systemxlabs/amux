/**
 * 方法处理器：把 shared 协议方法面接到 SessionManager / GitRunner / HarnessRegistry。
 * 参数校验在这里做（非法参数 → -32602；输入内容非法 → InvalidInputError → -32005）。
 */

import { InvalidInputError, SessionBusyError, type ContentBlock, type Input } from "ahal";
import { Methods, type HarnessName } from "shared";
import { InvalidParamsError } from "./errors.js";
import { GitRunner } from "./git.js";
import { HarnessRegistry } from "./harness.js";
import type { RpcHandler } from "./rpc.js";
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
      const extra: Record<string, unknown> = {};
      if (typeof b.mimeType === "string") extra.mimeType = b.mimeType;
      if (typeof b.title === "string") extra.title = b.title;
      if (typeof b.description === "string") extra.description = b.description;
      if (typeof b.size === "number") extra.size = b.size;
      out.push({ type: "resource_link", uri: b.uri, name: typeof b.name === "string" ? b.name : b.uri, ...extra });
    } else {
      throw new InvalidInputError(`未知内容块类型: ${String(b.type)}`);
    }
  }
  return out;
}

// ---- 方法表 ----

export function buildMethodHandlers(deps: HandlerDeps): Record<string, RpcHandler> {
  return {
    [Methods.GetInfo]: async () => ({
      info: { serverVersion: deps.serverVersion, harnesses: deps.harnesses.info() },
    }),

    [Methods.ListSessions]: async () => ({ sessions: deps.manager.list() }),

    [Methods.CreateSession]: async (params) => {
      const p = expectObject(params);
      const harness = str(p, "harness");
      if (!deps.harnesses.has(harness)) throw new InvalidParamsError(`未知 harness: ${harness}`);
      const session = await deps.manager.create({ harness: harness as HarnessName, cwd: str(p, "cwd"), model: optStr(p, "model") });
      return { session };
    },

    [Methods.ResumeSession]: async (params) => {
      const p = expectObject(params);
      const session = await deps.manager.resume(str(p, "sessionId"));
      return { session };
    },

    [Methods.CloseSession]: async (params) => {
      const p = expectObject(params);
      await deps.manager.close(str(p, "sessionId"));
      return null;
    },

    [Methods.DeleteSession]: async (params) => {
      const p = expectObject(params);
      await deps.manager.delete(str(p, "sessionId"));
      return null;
    },

    [Methods.Prompt]: async (params) => {
      const p = expectObject(params);
      const sessionId = str(p, "sessionId");
      const input = validateInput(p.input);
      await deps.manager.prompt(sessionId, input);
      return null;
    },

    [Methods.Cancel]: async (params) => {
      const p = expectObject(params);
      await deps.manager.cancel(str(p, "sessionId"));
      return null;
    },

    [Methods.GetHistory]: async (params) => {
      const p = expectObject(params);
      return { events: deps.manager.historyFor(str(p, "sessionId")) };
    },

    [Methods.GetBufferedEvents]: async (params) => {
      const p = expectObject(params);
      const afterSeq = p.afterSeq;
      if (afterSeq !== undefined && (typeof afterSeq !== "number" || !Number.isInteger(afterSeq))) {
        throw new InvalidParamsError("参数 afterSeq 必须是整数");
      }
      return { events: deps.manager.bufferedFor(str(p, "sessionId"), afterSeq as number | undefined) };
    },

    [Methods.GitStatus]: async (params) => {
      const p = expectObject(params);
      return deps.git.status(str(p, "cwd"));
    },

    [Methods.GitDiff]: async (params) => {
      const p = expectObject(params);
      const diff = await deps.git.diff(str(p, "cwd"), optStr(p, "path"));
      return { diff };
    },

    [Methods.GitPush]: async (params) => {
      const p = expectObject(params);
      return deps.git.push(str(p, "cwd"));
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
      return deps.git.revert(cwd, { path, patch });
    },
  };
}
