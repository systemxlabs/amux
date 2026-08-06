/**
 * 会话注册表：磁盘持久化的会话元数据（~/.amux/server/sessions.json）。
 * interrupted = 崩溃恢复标记（上次非正常退出且忙，或恢复失败），由客户端决定是否 resume。
 */

import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import type { SessionState } from "ahal";
import type { HarnessName, SessionMeta } from "shared";

export interface RegisteredSession {
  id: string;
  /** harness 侧会话 id（driver Session.id，resume 键）；创建时记录，恢复时使用 */
  harnessSessionId?: string;
  harness: HarnessName;
  cwd: string;
  model?: string;
  createdAt: number;
  lastEventAt: number;
  lastState: SessionState;
  /** 已持久化/缓冲事件的最后 seq（重启后新事件从 lastSeq+1 延续） */
  lastSeq: number;
  closed: boolean;
  interrupted: boolean;
}

export function toMeta(e: RegisteredSession): SessionMeta {
  return {
    id: e.id,
    harness: e.harness,
    cwd: e.cwd,
    model: e.model,
    state: e.lastState,
    interrupted: e.interrupted,
    closed: e.closed,
    createdAt: e.createdAt,
    lastEventAt: e.lastEventAt,
  };
}

export class SessionRegistry {
  private entries = new Map<string, RegisteredSession>();

  constructor(private readonly file: string) {}

  load(): void {
    if (!existsSync(this.file)) return;
    try {
      const data = JSON.parse(readFileSync(this.file, "utf8")) as { sessions?: RegisteredSession[] };
      for (const s of data.sessions ?? []) this.entries.set(s.id, s);
    } catch {
      // 文件损坏：从空注册表开始（不覆盖原文件，直到下次保存）
    }
  }

  get(id: string): RegisteredSession | undefined {
    return this.entries.get(id);
  }

  all(): RegisteredSession[] {
    return [...this.entries.values()];
  }

  has(id: string): boolean {
    return this.entries.has(id);
  }

  upsert(e: RegisteredSession): void {
    this.entries.set(e.id, e);
  }

  remove(id: string): void {
    this.entries.delete(id);
  }

  /** 原子写盘（tmp + rename）。 */
  save(): void {
    mkdirSync(dirname(this.file), { recursive: true });
    const tmp = this.file + ".tmp";
    writeFileSync(tmp, JSON.stringify({ sessions: [...this.entries.values()] }, null, 2));
    renameSync(tmp, this.file);
  }
}
