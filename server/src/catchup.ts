/**
 * 按连接补齐（docs/DESIGN.md §3.1、§5）：
 * server 记录每个连接在各会话上的补齐位置；补齐完成（get_history）前到达的
 * 会话流通知（event / user_message）按连接暂存，历史返回后按序补齐缺口再并入
 * 实时广播——客户端按序追加即可，天然无重复、无需去重。
 *
 * 本模块为纯逻辑（不依赖传输层）：补齐位置用每会话内部单调序号（server 分配，
 * 不进协议、不落盘；重启后从 1 重新开始即可，历史顺序由 jsonl 追加次序保证）。
 */

export interface CatchupItem {
  /** 内部序号（server 分配，仅用于补齐位置对齐） */
  order: number;
  method: "event" | "user_message";
  params: unknown;
}

export type RouteAction = "deliver" | "stash" | "skip";

/** 单会话暂存上限（防御：客户端永不 get_history 的异常场景，避免无限增长）。 */
const MAX_PENDING_PER_SESSION = 5000;

export class ConnectionCatchup {
  private readonly pos = new Map<string, number>();
  private readonly pending = new Map<string, CatchupItem[]>();

  /**
   * 会话流通知路由：
   * - 该会话尚未补齐（未 get_history）→ stash（按到达顺序暂存）
   * - 已补齐且 order 大于补齐位置 → deliver（正常实时广播）
   * - 已补齐但 order 不大于补齐位置 → skip（历史已包含）
   */
  route(sessionId: string, order: number, method: "event" | "user_message", params: unknown): RouteAction {
    const p = this.pos.get(sessionId);
    if (p === undefined) {
      const list = this.pending.get(sessionId) ?? [];
      list.push({ order, method, params });
      if (list.length > MAX_PENDING_PER_SESSION) list.shift(); // 防御上限：丢弃最旧
      this.pending.set(sessionId, list);
      return "stash";
    }
    return order > p ? "deliver" : "skip";
  }

  /**
   * get_history 时标记该会话的补齐位置；返回暂存中 order 大于 pos 的项
   * （按到达顺序 = 内部序号顺序），并清空暂存。调用方须保证历史快照与该位置
   * 原子对齐（读盘与标记在同一同步块内）。
   *
   * 说明：在当前实现（先落盘、后分配 order，快照读与位置标记同步原子）下，
   * 暂存项（连接建立后、get_history 前到达）必已被历史快照覆盖，本方法恒返回
   * 空数组——补齐语义由原子快照保证，缺口仅含不落盘事件（chunk/state），由
   * 完整消息收敛与 meta 同步兜底。返回过滤逻辑保留为防御（防止未来改动破坏
   * 该不变量时静默丢事件）。
   */
  mark(sessionId: string, pos: number): CatchupItem[] {
    this.pos.set(sessionId, pos);
    const list = this.pending.get(sessionId) ?? [];
    this.pending.delete(sessionId);
    return list.filter((it) => it.order > pos);
  }

  /** 连接关闭时释放全部状态。 */
  clear(): void {
    this.pos.clear();
    this.pending.clear();
  }
}
