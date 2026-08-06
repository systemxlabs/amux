/**
 * 会话历史存储：对话相关事件（非 *_chunk）按会话追加写入 JSONL（全量持久化）。
 * 每个事件带 server 序号（seq），与缓冲/实时流共用同一序号空间。
 */

import { existsSync, mkdirSync, readFileSync } from "node:fs";
import { appendFile, rm } from "node:fs/promises";
import { join } from "node:path";
import type { StoredEvent } from "shared";

export class HistoryStore {
  constructor(private readonly dir: string) {}

  private file(sessionId: string): string {
    return join(this.dir, `${sessionId}.jsonl`);
  }

  async append(sessionId: string, ev: StoredEvent): Promise<void> {
    mkdirSync(this.dir, { recursive: true });
    await appendFile(this.file(sessionId), JSON.stringify(ev) + "\n", "utf8");
  }

  load(sessionId: string): StoredEvent[] {
    const f = this.file(sessionId);
    if (!existsSync(f)) return [];
    const out: StoredEvent[] = [];
    for (const line of readFileSync(f, "utf8").split("\n")) {
      if (!line.trim()) continue;
      try {
        out.push(JSON.parse(line) as StoredEvent);
      } catch {
        // 跳过损坏行
      }
    }
    return out;
  }

  lastSeq(sessionId: string): number {
    const evs = this.load(sessionId);
    return evs.length ? evs[evs.length - 1].seq : -1;
  }

  async remove(sessionId: string): Promise<void> {
    await rm(this.file(sessionId), { force: true });
  }
}
