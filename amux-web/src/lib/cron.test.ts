import { describe, expect, it } from "vitest";

import { previewCron } from "./cron";

describe("previewCron", () => {
  it("返回最近 5 次递增的触发时间", () => {
    const preview = previewCron("0 9 * * *", new Date("2026-01-01T00:00:00"));

    expect(preview.error).toBeNull();
    expect(preview.nextRuns).toHaveLength(5);
    expect(
      preview.nextRuns.every(
        (run, index) => index === 0 || run.getTime() > preview.nextRuns[index - 1].getTime(),
      ),
    ).toBe(true);
  });

  it("非法表达式返回错误且不展示触发时间", () => {
    const preview = previewCron("not a cron");

    expect(preview.error).not.toBeNull();
    expect(preview.nextRuns).toEqual([]);
  });
});
