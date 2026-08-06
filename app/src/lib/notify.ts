/**
 * 通知推导（纯逻辑）：从事件流推导「工作结束 / 异常 / 长时间无响应」。
 * 配置存客户端本地（docs/DESIGN.md §4：通知由客户端从事件流自行推导）。
 */

import type { Event } from "ahal";

export interface NotifyConfig {
  /** 工作结束（含结束原因）时通知 */
  workEnded: boolean;
  /** error 事件时通知 */
  onError: boolean;
  /** 忙且超过该秒数无事件时通知；0 = 关闭 */
  longIdleSeconds: number;
}

export type AppNotification =
  | { kind: "work-ended"; reason?: string }
  | { kind: "error"; message: string }
  | { kind: "long-idle" };

export class NotificationDetector {
  private busySince: number | null = null;
  private lastEventAt = 0;
  private lastIdleAlert = 0;

  constructor(
    private readonly config: NotifyConfig,
    private readonly now: () => number = Date.now,
  ) {}

  onEvent(event: Event, timestamp: number): AppNotification | null {
    this.lastEventAt = Math.max(this.lastEventAt, timestamp);
    switch (event.kind) {
      case "state_changed": {
        if (event.state !== "idle") {
          if (this.busySince === null) this.busySince = timestamp;
          return null;
        }
        const wasBusy = this.busySince !== null;
        this.busySince = null;
        if (wasBusy && this.config.workEnded) return { kind: "work-ended", reason: event.reason };
        return null;
      }
      case "error":
        if (this.config.onError) return { kind: "error", message: event.message };
        return null;
      default:
        return null;
    }
  }

  /** 心跳：忙且超过阈值无事件 → 长时间无响应通知（防刷）。 */
  tick(now = this.now()): AppNotification | null {
    if (this.config.longIdleSeconds <= 0 || this.busySince === null) return null;
    if (now - this.lastEventAt < this.config.longIdleSeconds * 1000) return null;
    if (now - this.lastIdleAlert < this.config.longIdleSeconds * 1000) return null;
    this.lastIdleAlert = now;
    return { kind: "long-idle" };
  }
}
