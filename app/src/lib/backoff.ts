/**
 * 指数退避重连延迟计算（纯函数，无随机依赖——随机源可注入以便测试）。
 * 语义依据：docs/DESIGN.md §3「断线后客户端指数退避重连」。
 */

export interface BackoffConfig {
  baseMs: number;
  maxMs: number;
  /** 每轮尝试的倍数，默认 2 */
  factor?: number;
  /** 抖动比例（0..1，乘性抖动），默认 0.1；0 = 无抖动 */
  jitter?: number;
  /** 可注入随机源（测试用），默认 Math.random */
  random?: () => number;
}

export function nextBackoffDelay(attempt: number, cfg: BackoffConfig): number {
  const factor = cfg.factor ?? 2;
  const jitter = cfg.jitter ?? 0.1;
  const rnd = cfg.random ?? Math.random;
  const raw = Math.min(cfg.maxMs, cfg.baseMs * Math.pow(factor, Math.max(0, attempt)));
  const j = jitter > 0 ? 1 + (rnd() * 2 - 1) * jitter : 1;
  return Math.max(1, Math.round(raw * j));
}
