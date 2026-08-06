import { describe, expect, it } from "vitest";
import { nextBackoffDelay } from "./backoff.js";

describe("nextBackoffDelay（指数退避）", () => {
  it("第 0 次尝试为 base，随尝试次数指数增长", () => {
    const cfg = { baseMs: 500, maxMs: 30000, jitter: 0, random: () => 0.5 };
    expect(nextBackoffDelay(0, cfg)).toBe(500);
    expect(nextBackoffDelay(1, cfg)).toBe(1000);
    expect(nextBackoffDelay(2, cfg)).toBe(2000);
    expect(nextBackoffDelay(3, cfg)).toBe(4000);
  });

  it("封顶于 maxMs", () => {
    const cfg = { baseMs: 1000, maxMs: 5000, jitter: 0, random: () => 0.5 };
    expect(nextBackoffDelay(10, cfg)).toBe(5000);
    expect(nextBackoffDelay(20, cfg)).toBe(5000);
  });

  it("抖动在 ±jitter 范围内", () => {
    const cfg = { baseMs: 1000, maxMs: 100000, jitter: 0.2, random: () => 1 };
    expect(nextBackoffDelay(0, cfg)).toBe(1200);
    const cfg0 = { baseMs: 1000, maxMs: 100000, jitter: 0.2, random: () => 0 };
    expect(nextBackoffDelay(0, cfg0)).toBe(800);
  });

  it("负尝试次数视为 0", () => {
    const cfg = { baseMs: 500, maxMs: 10000, jitter: 0, random: () => 0.5 };
    expect(nextBackoffDelay(-1, cfg)).toBe(500);
  });
});
