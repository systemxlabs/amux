/**
 * 会话历史存储：每个会话一个 JSONL 文件（`<sessionId>.jsonl`），
 * agent 事件与用户输入**混合按序**写入（docs/DESIGN.md §5）——
 * 一行一条记录，按追加顺序即真实对话顺序（jsonl 只追加不修改）。
 * 事件侧只落非 *_chunk 的完整事件（对话内容，全量持久化）。
 */

import { existsSync, mkdirSync, readFileSync } from "node:fs";
import { appendFile, rm } from "node:fs/promises";
import { join } from "node:path";
import type { HistoryItem } from "shared";

export class HistoryStore {
  constructor(private readonly dir: string) {}

  private file(sessionId: string): string {
    return join(this.dir, `${sessionId}.jsonl`);
  }

  async append(sessionId: string, item: HistoryItem): Promise<void> {
    mkdirSync(this.dir, { recursive: true });
    await appendFile(this.file(sessionId), JSON.stringify(item) + "\n", "utf8");
  }

  load(sessionId: string): HistoryItem[] {
    const f = this.file(sessionId);
    if (!existsSync(f)) return [];
    const out: HistoryItem[] = [];
    for (const line of readFileSync(f, "utf8").split("\n")) {
      if (!line.trim()) continue;
      try {
        out.push(JSON.parse(line) as HistoryItem);
      } catch {
        // 跳过损坏行
      }
    }
    return out;
  }

  async remove(sessionId: string): Promise<void> {
    await rm(this.file(sessionId), { force: true });
  }
}
