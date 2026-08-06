/**
 * 有界流式事件缓冲：按会话保留最近 N 条原始事件（非全量持久化），
 * 供重连客户端补齐"历史快照"与"实时流"之间的缺口。
 */

import type { StoredEvent } from "shared";

export class EventBuffer {
  private readonly bySession = new Map<string, StoredEvent[]>();

  constructor(private readonly maxPerSession: number) {}

  push(sessionId: string, ev: StoredEvent): void {
    let list = this.bySession.get(sessionId);
    if (!list) {
      list = [];
      this.bySession.set(sessionId, list);
    }
    list.push(ev);
    if (list.length > this.maxPerSession) {
      list.splice(0, list.length - this.maxPerSession);
    }
  }

  /** 取某会话缓冲中 seq 大于 afterSeq 的事件（缺省取全部）。 */
  get(sessionId: string, afterSeq = -1): StoredEvent[] {
    const list = this.bySession.get(sessionId);
    if (!list) return [];
    return list.filter((e) => e.seq > afterSeq);
  }

  clear(sessionId: string): void {
    this.bySession.delete(sessionId);
  }
}
