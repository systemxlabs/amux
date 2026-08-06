/**
 * 会话事件流合并（纯逻辑）：历史（全量对话）+ 缓冲/实时（带 seq）合并，
 * 以 seq 去重——缺口事件恰好一次、不重复（docs/DESIGN.md §3.1 连接补齐）。
 */

import type { Event } from "ahal";

export interface FeedEvent {
  seq: number;
  event: Event;
  timestamp: number;
}

export class SessionFeed {
  private items: FeedEvent[] = [];
  private seen = -1;
  private rev = 0;

  constructor(public readonly sessionId: string) {}

  /** 已合并的按序事件（历史在前，缺口/实时在后） */
  get events(): readonly FeedEvent[] {
    return this.items;
  }

  get lastSeq(): number {
    return this.seen;
  }

  /**
   * 内容版本号：每次变更自增。
   * events 数组是原地变更的（引用不变），React 的 useMemo 依赖数组引用
   * 无法感知新增事件——UI 层须依赖本字段触发重算。
   */
  get revision(): number {
    return this.rev;
  }

  /** 重连补齐第一步：以历史替换当前内容。 */
  applyHistory(events: readonly FeedEvent[]): void {
    this.items = [...events];
    this.seen = events.length ? events[events.length - 1].seq : -1;
    this.rev++;
  }

  /** 应用一批事件（缓冲补齐或实时到达）；返回实际新增条数。 */
  apply(events: readonly FeedEvent[]): number {
    let added = 0;
    for (const e of events) {
      if (this.applyOne(e)) added++;
    }
    return added;
  }

  /** 应用单条；seq <= 已见最大 seq 的事件跳过（去重）。 */
  applyOne(e: FeedEvent): boolean {
    if (e.seq <= this.seen) return false;
    this.items.push(e);
    this.seen = e.seq;
    this.rev++;
    return true;
  }
}
