// cron 表达式校验与触发时间预览（docs/PRD.md「新建自动化视图」）。

import { Cron } from "croner";

export type CronPreview = {
  error: string | null;
  nextRuns: Date[];
};

export function previewCron(expression: string, from?: Date): CronPreview {
  const value = expression.trim();
  if (value === "") {
    return { error: "请输入 cron 表达式", nextRuns: [] };
  }
  try {
    const nextRuns = new Cron(value).nextRuns(5, from);
    if (nextRuns.length < 5) {
      return { error: "无法计算 cron 表达式最近 5 次触发时间", nextRuns: [] };
    }
    return { error: null, nextRuns };
  } catch (error) {
    return {
      error: `cron 表达式无效：${error instanceof Error ? error.message : String(error)}`,
      nextRuns: [],
    };
  }
}
