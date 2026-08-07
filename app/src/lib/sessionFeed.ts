/**
 * 会话流（纯逻辑）：历史 + 实时合并。
 * 事件交付按 docs/DESIGN.md §3.1/§5——server 按连接对齐，客户端按到达顺序
 * 追加即可，天然无重复、无需去重（无 seq 游标）。
 * 用户消息由 server 自身存储（不进 AHAL 事件流），在 feed 里与事件同序展示。
 * 通知推导用「实时事件队列」：只有以通知到达的实时/补齐项入队，历史不触发通知。
 */

import type { ContentBlock, Event } from "ahal";

export type FeedItem =
  | { event: Event; timestamp: number }
  | { content: ContentBlock[]; timestamp: number };

export class SessionFeed {
  private items: FeedItem[] = [];
  private notifyQueue: FeedItem[] = [];
  private rev = 0;

  constructor(public readonly sessionId: string) {}

  /** 已合并的按序项（历史在前，实时在后） */
  get events(): readonly FeedItem[] {
    return this.items;
  }

  /**
   * 内容版本号：每次变更自增。
   * items 数组是原地变更的（引用不变），React 的 useMemo 依赖数组引用
   * 无法感知新增事件——UI 层须依赖本字段触发重算。
   */
  get revision(): number {
    return this.rev;
  }

  /** 取走自上次调用以来以通知到达的实时事件（通知推导用；消费即清空）。 */
  drainNotify(): FeedItem[] {
    const out = this.notifyQueue;
    this.notifyQueue = [];
    return out;
  }

  /** 重连补齐：以历史（按 jsonl 追加顺序）替换当前内容。 */
  applyHistory(items: readonly FeedItem[]): void {
    this.items = [...items];
    this.rev++;
  }

  /** 应用一批实时项（按到达顺序追加）。 */
  apply(items: readonly FeedItem[]): number {
    for (const item of items) this.applyOne(item);
    return items.length;
  }

  /** 应用单个实时项（按序追加；server 按连接对齐保证无重复，无需去重）。 */
  applyOne(item: FeedItem): boolean {
    this.items.push(item);
    this.notifyQueue.push(item);
    this.rev++;
    return true;
  }
}
